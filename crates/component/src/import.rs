//! Imports: a profile-driven connector job, or a file a person chose in the Explorer.
use crate::attricat::host::api;
use crate::attricat::host::outcomes::{Failure, Retry};
use crate::attricat::host::{artifacts, attricat, attricat_data, clock, selection};
use crate::exports::attricat::host::deferrable_operations::{BatchOutcome, OperationRequest};
use crate::{checkpoint, diagnostic, parse, progress, rejected, retry, schema, state};
use attricat_csv_core::{
    CsvReader, EmptyPolicy, Error, ImportProfile, MappedRow, Mapping, Plan, Position,
    UnknownHeaders, check_schema, columns_from_headers, report_rows,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;

/// The output that lists rejected rows.
const REPORT: &str = "rejections.csv";
/// Rows and parsed bytes per batch. Both are fixed so a replayed batch covers
/// the same rows and appends the same report bytes.
const BATCH_ROWS: usize = 40;
const BATCH_BYTES: u64 = 512 * 1024;
/// Rows per host write, and how close JSON may come to the 64 KiB request bound.
const WRITE_ROWS: usize = 10;
const WRITE_BYTES: usize = 60 * 1024;
/// A call has 10 seconds, and going over quarantines the extension. Writes stop
/// early enough that one more, of slow rows, still ends within the call.
const READ_BUDGET_MS: u64 = 5_000;
const WRITE_BUDGET_MS: u64 = 6_500;
const READ_BUDGET_ERROR: &str = "input read budget exceeded";
const REJECTED_INTENTS: &str = "one or more catalog intents were rejected";
const TOO_LARGE_ROW: &str = "this row is too large to write";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportInput {
    profile: ImportProfile,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct State {
    position: Position,
    rows: u64,
    imported: u64,
    rejected: u64,
}

fn now_ms() -> u64 {
    let now = clock::now();
    now.seconds * 1000 + u64::from(now.nanoseconds) / 1_000_000
}

/// The run's input. A long way into a large file, reaching the position takes
/// most of a call, because a host read cannot seek. Stopping here ends the run
/// with a reason instead of running out the call and quarantining the extension.
struct Source {
    handle: artifacts::InputArtifact,
    deadline: u64,
}
impl Source {
    fn open() -> Result<Self, Failure> {
        Ok(Self {
            handle: artifacts::open_input("source").map_err(|error| retry(&error))?,
            deadline: now_ms() + READ_BUDGET_MS,
        })
    }
}
impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if now_ms() > self.deadline {
            return Err(std::io::Error::other(READ_BUDGET_ERROR));
        }
        let size = out.len().min(attricat_csv_core::MAX_RECORD_BYTES) as u32;
        let bytes = artifacts::read(&self.handle, size).map_err(std::io::Error::other)?;
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

/// What a read failure means: a problem with the file ends the run; a problem
/// reaching it is retried.
fn read_failure(error: Error) -> Failure {
    let message = error.to_string();
    match error {
        _ if message.contains(READ_BUDGET_ERROR) => rejected(
            "input_too_large",
            "This file is too large to import in one run. Split it into smaller files.",
        ),
        Error::Header(message) => rejected("invalid_csv", &message),
        Error::Csv(_) if message.contains("64 KiB") => rejected("invalid_csv", &message),
        _ => retry(&message),
    }
}

/// Applied rows answer as already applied when a batch runs again, so waiting
/// makes progress without spending the failure budget.
fn check_deadline(deadline: u64) -> Result<(), Failure> {
    if now_ms() > deadline {
        return Err(Failure::Retry(Retry {
            message: diagnostic("import batch is taking longer than one call allows"),
            after_ms: Some(1000),
        }));
    }
    Ok(())
}

trait Writer {
    /// Writes the rows, returning those it rejected.
    fn write(&self, rows: &[MappedRow], deadline: u64) -> Result<Vec<(u64, String)>, Failure>;
}

/// Rows of a connector job or an administrative run, through `upsert-batch`.
struct ProfileWriter<'a> {
    request: &'a OperationRequest,
    profile: &'a ImportProfile,
}
impl ProfileWriter<'_> {
    fn send(&self, rows: &[MappedRow]) -> Result<(), String> {
        let intents: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "row": row.row,
                    "business_key": self.profile.business_key,
                    "key": row.key,
                    "values": row.values,
                })
            })
            .collect();
        let body = json!({
            "blueprint_id": self.profile.blueprint_id,
            "blueprint_version": self.profile.blueprint_version,
            "context_id": self.profile.context_id,
            "run_id": self.request.run_id,
            "batch_key": self.request.batch_key,
            "intents": intents,
        });
        attricat::upsert_batch(&body.to_string())
    }
}
impl Writer for ProfileWriter<'_> {
    fn write(&self, rows: &[MappedRow], deadline: u64) -> Result<Vec<(u64, String)>, Failure> {
        match self.send(rows) {
            Ok(()) => Ok(Vec::new()),
            // The answer doesn't say which row failed. Rows that were applied
            // answer as already applied, so sending each alone finds them.
            Err(error) if error.contains(REJECTED_INTENTS) => {
                let mut rejections = Vec::new();
                for row in rows {
                    check_deadline(deadline)?;
                    match self.send(std::slice::from_ref(row)) {
                        Ok(()) => {}
                        Err(error) if error.contains(REJECTED_INTENTS) => rejections.push((
                            row.row,
                            "Attricat rejected this row; check its values against the blueprint"
                                .to_string(),
                        )),
                        Err(error) => return Err(retry(&error)),
                    }
                }
                Ok(rejections)
            }
            Err(error) => Err(retry(&error)),
        }
    }
}

/// Rows of an Explorer import, with per-row outcomes from `attricat-data.batch`.
struct FileWriter<'a> {
    request: &'a OperationRequest,
    blueprint_id: &'a str,
    blueprint_version: u64,
    context_id: &'a str,
    lookup_attribute_id: Option<&'a str>,
}
#[derive(Deserialize)]
struct Outcome {
    intent_key: String,
    status: String,
    error: Option<String>,
}
impl FileWriter<'_> {
    fn intent(&self, row: &MappedRow) -> Value {
        let values: Vec<Value> = row
            .values
            .iter()
            .map(|(code, value)| {
                json!({
                    "kind": "scalar",
                    "attribute_code": code,
                    "context_id": self.context_id,
                    "value": value,
                })
            })
            .collect();
        let mut intent = json!({
            "kind": "create",
            "intent_key": format!("row-{}", row.row),
            "blueprint_id": self.blueprint_id,
            "blueprint_version": self.blueprint_version,
            "values": values,
        });
        if let (Some(attribute), Some(key)) = (self.lookup_attribute_id, &row.key) {
            intent["kind"] = json!("upsert");
            intent["lookup_attribute_id"] = json!(attribute);
            intent["lookup_value"] = json!(key);
        }
        intent
    }
}
impl Writer for FileWriter<'_> {
    fn write(&self, rows: &[MappedRow], _deadline: u64) -> Result<Vec<(u64, String)>, Failure> {
        let intents: Vec<Value> = rows.iter().map(|row| self.intent(row)).collect();
        let body = json!({"operation": "batch", "batch": {
            "batch_key": self.request.batch_key,
            "dry_run": false,
            "intents": intents,
        }});
        let raw = attricat_data::batch(&body.to_string()).map_err(|error| retry(&error))?;
        let outcomes: Vec<Outcome> = parse(&raw, "invalid host batch outcome")?;
        Ok(outcomes
            .into_iter()
            .filter(|outcome| outcome.status == "rejected")
            .map(|outcome| {
                let row = outcome.intent_key.trim_start_matches("row-");
                (
                    row.parse().unwrap_or_default(),
                    outcome
                        .error
                        .unwrap_or_else(|| "Attricat rejected this row".into()),
                )
            })
            .collect())
    }
}

/// Reads one batch of rows, writes them, and reports those rejected.
fn run_batch(
    request: &OperationRequest,
    mut s: State,
    mut reader: CsvReader<Source>,
    plan: &Plan,
    writer: Option<&dyn Writer>,
    deadline: u64,
) -> Result<BatchOutcome, Failure> {
    let first_batch = s.rejected == 0;
    let start = reader.position().offset;
    let mut pending: Vec<MappedRow> = Vec::new();
    let mut rejections: Vec<(u64, String)> = Vec::new();
    let mut read = 0;
    let mut done = false;
    let write_bytes =
        |row: &MappedRow| serde_json::to_vec(&row.values).map_or(usize::MAX, |v| v.len());
    while read < BATCH_ROWS && reader.position().offset - start < BATCH_BYTES {
        let record = match reader.next_record() {
            Ok(Some(record)) => record,
            Ok(None) => {
                done = true;
                break;
            }
            Err(error) => return Err(read_failure(error)),
        };
        read += 1;
        match reader.map(plan, &record) {
            // Attribute codes and values are all a row sends beyond a small
            // envelope, so this is its size within a write.
            Ok(row) if write_bytes(&row) > WRITE_BYTES => {
                rejections.push((row.row, TOO_LARGE_ROW.into()))
            }
            Ok(row) => pending.push(row),
            Err(Error::Row { row, message }) => rejections.push((row, message)),
            Err(error) => return Err(read_failure(error)),
        }
    }
    let attempted = pending.len() as u64;
    let mut refused: Vec<(u64, String)> = Vec::new();
    if let Some(writer) = writer {
        let mut chunk: Vec<MappedRow> = Vec::new();
        let mut size = 0;
        let mut flush = |chunk: &mut Vec<MappedRow>| -> Result<(), Failure> {
            if chunk.is_empty() {
                return Ok(());
            }
            check_deadline(deadline)?;
            refused.extend(writer.write(chunk, deadline)?);
            chunk.clear();
            Ok(())
        };
        for row in pending {
            let bytes = write_bytes(&row);
            if chunk.len() == WRITE_ROWS || size + bytes > WRITE_BYTES {
                flush(&mut chunk)?;
                size = 0;
            }
            size += bytes;
            chunk.push(row);
        }
        flush(&mut chunk)?;
    }
    s.imported += attempted - refused.len() as u64;
    rejections.append(&mut refused);
    if !rejections.is_empty() {
        rejections.sort_by_key(|(row, _)| *row);
        let data = report_rows(first_batch, &rejections).map_err(|e| retry(&e.to_string()))?;
        artifacts::append_output(REPORT, "text/csv", &request.batch_key, &data)
            .map_err(|error| retry(&error))?;
    }
    s.position = reader.position();
    s.rows += read as u64;
    s.rejected += rejections.len() as u64;
    Ok(BatchOutcome {
        checkpoint: checkpoint(&s)?,
        progress: progress(s.rows, s.imported, s.rejected, None),
        done,
        resume_after_ms: None,
    })
}

fn input(request: &OperationRequest) -> Result<ImportInput, Failure> {
    let input: ImportInput = serde_json::from_str(&request.input)
        .map_err(|error| rejected("invalid_input", &format!("invalid import input: {error}")))?;
    input
        .profile
        .validate()
        .map_err(|error| rejected("invalid_input", &error.to_string()))?;
    Ok(input)
}

pub fn validate(request: &OperationRequest) -> Result<(), Failure> {
    let input = input(request)?;
    let profile = &input.profile;
    let attributes = schema(
        &profile.blueprint_id,
        profile.blueprint_version,
        &profile.context_id,
    )?;
    check_schema(&profile.columns, &attributes)
        .map_err(|error| rejected("invalid_input", &error.to_string()))
}

pub fn profile_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    let input = input(request)?;
    let s: State = state(request)?;
    let deadline = now_ms() + WRITE_BUDGET_MS;
    let reader = CsvReader::open(Source::open()?, s.position).map_err(read_failure)?;
    let plan = Plan::new(Mapping::from(&input.profile), reader.headers())
        .map_err(|error| rejected("invalid_csv", &error.to_string()))?;
    let writer = ProfileWriter {
        request,
        profile: &input.profile,
    };
    let writer: Option<&dyn Writer> = (!input.dry_run).then_some(&writer);
    run_batch(request, s, reader, &plan, writer, deadline)
}

/// The attribute that identifies records of a blueprint, chosen in the import
/// settings dialog. Attribute IDs differ per blueprint revision.
#[derive(Deserialize)]
struct ImportKey {
    attribute_code: String,
    attribute_ids: std::collections::HashMap<String, String>,
}

fn import_key(blueprint_id: &str) -> Result<Option<ImportKey>, Failure> {
    #[derive(Deserialize)]
    struct Entry {
        value: ImportKey,
    }
    let request = json!({ "key": format!("import-key:{blueprint_id}") }).to_string();
    let raw = api::call("storage.get.v1", &request).map_err(|error| retry(&error))?;
    Ok(parse::<Option<Entry>>(&raw, "invalid import settings")?.map(|entry| entry.value))
}

/// A person sees only that a failed run failed, and can't download anything from
/// it. So a reason to stop importing a file completes the run with a report.
pub fn file_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    match read_file_batch(request) {
        Err(Failure::Rejected(rejection)) => {
            let mut s: State = state(request)?;
            let reason: Value = parse(&rejection.message, "invalid failure")?;
            let reason = reason["error"].as_str().unwrap_or(&rejection.code);
            let data = report_rows(s.rejected == 0, &[(s.position.row.max(1), reason.into())])
                .map_err(|error| retry(&error.to_string()))?;
            artifacts::append_output(REPORT, "text/csv", &request.batch_key, &data)
                .map_err(|error| retry(&error))?;
            s.rejected += 1;
            Ok(BatchOutcome {
                checkpoint: checkpoint(&s)?,
                progress: progress(s.rows, s.imported, s.rejected, None),
                done: true,
                resume_after_ms: None,
            })
        }
        outcome => outcome,
    }
}

fn read_file_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    #[derive(Deserialize)]
    struct Described {
        blueprint_id: String,
        blueprint_version: u64,
        context_id: Option<String>,
        default_context_id: String,
    }
    let deadline = now_ms() + WRITE_BUDGET_MS;
    let raw = selection::describe().map_err(|error| retry(&error))?;
    let run: Described = parse(&raw, "invalid host selection")?;
    let context_id = run.context_id.as_deref().unwrap_or(&run.default_context_id);
    let s: State = state(request)?;
    let reader = CsvReader::open(Source::open()?, s.position).map_err(read_failure)?;
    let attributes = schema(&run.blueprint_id, run.blueprint_version, context_id)?;
    let columns = columns_from_headers(reader.headers(), &attributes)
        .map_err(|error| rejected("invalid_csv", &error.to_string()))?;
    // A chosen key that can't be used fails the run: creating instead would
    // duplicate the records the person meant to update.
    let key = import_key(&run.blueprint_id)?;
    let lookup_attribute_id = match &key {
        None => None,
        Some(key) => {
            if !columns.iter().any(|c| c.attribute == key.attribute_code) {
                return Err(rejected(
                    "invalid_csv",
                    &format!(
                        "This blueprint's import settings match records by {0}, so the file needs a {0} column.",
                        key.attribute_code
                    ),
                ));
            }
            let id = key.attribute_ids.get(&run.blueprint_version.to_string());
            Some(id.ok_or_else(|| {
                rejected(
                    "import_settings_outdated",
                    "This blueprint has a new revision. Open CSV import settings and save them again.",
                )
            })?)
        }
    };
    let mapping = Mapping {
        columns,
        business_key: key.as_ref().map(|key| key.attribute_code.clone()),
        unknown_headers: UnknownHeaders::Reject,
        empty: EmptyPolicy::Skip,
    };
    let plan = Plan::new(mapping, reader.headers())
        .map_err(|error| rejected("invalid_csv", &error.to_string()))?;
    let writer = FileWriter {
        request,
        blueprint_id: &run.blueprint_id,
        blueprint_version: run.blueprint_version,
        context_id,
        lookup_attribute_id: lookup_attribute_id.map(String::as_str),
    };
    run_batch(request, s, reader, &plan, Some(&writer), deadline)
}

pub fn finish(request: &OperationRequest) -> Result<(), String> {
    let s: State = serde_json::from_str(&request.checkpoint).unwrap_or_default();
    if s.rejected > 0 {
        artifacts::finalize_output(REPORT)?;
    }
    Ok(())
}
