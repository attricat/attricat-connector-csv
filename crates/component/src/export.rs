//! Exports: a profile-driven connector job, or the records a person selected.
use crate::attricat::host::outcomes::Failure;
use crate::attricat::host::{api, artifacts, attricat, selection};
use crate::exports::attricat::host::deferrable_operations::{BatchOutcome, OperationRequest};
use crate::{checkpoint, parse, progress, rejected, retry, schema, state};
use attricat_csv_core::{ExportProfile, check_schema, encode_rows, export_row, selection_cell};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};

/// The output a person downloads.
const OUTPUT: &str = "export.csv";
const MAX_CHUNK_BYTES: usize = 65536;
/// The most rows `attricat.page` and `page-v2` return.
const PAGE_ROWS: u32 = 16;
/// The most members `selection.page` returns.
const SELECTION_ROWS: u32 = 10;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportInput {
    profile: ExportProfile,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct State {
    rows: u64,
    failed: u64,
    cursor: String,
    header: bool,
    /// The columns of a selection export, once read.
    columns: Vec<String>,
}

fn append(request: &OperationRequest, bytes: &[u8]) -> Result<(), Failure> {
    if bytes.is_empty() {
        return Ok(());
    }
    artifacts::append_output(OUTPUT, "text/csv", &request.batch_key, bytes)
        .map_err(|error| retry(&error))
}

fn input(request: &OperationRequest) -> Result<ExportInput, Failure> {
    let input: ExportInput = serde_json::from_str(&request.input)
        .map_err(|error| rejected("invalid_input", &format!("invalid export input: {error}")))?;
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

#[derive(Deserialize)]
struct Page<R> {
    rows: Vec<R>,
    next_cursor: Option<String>,
}
#[derive(Deserialize)]
struct RowV2 {
    #[serde(default)]
    values: HashMap<String, Value>,
}

/// One page of values. A connector run's `page-v2` resolves values through the
/// context's fallback chain and is limited to the run's channel; a run outside
/// a job has only `page`, with the values set in the context itself.
fn read_page(
    profile: &ExportProfile,
    scoped: bool,
    cursor: &str,
    limit: u32,
) -> Result<Page<HashMap<String, Value>>, Failure> {
    if scoped {
        let request = json!({ "cursor": cursor, "limit": limit }).to_string();
        let raw = attricat::page_v2(&request).map_err(|error| retry(&error))?;
        let page: Page<RowV2> = parse(&raw, "invalid host page")?;
        return Ok(Page {
            rows: page.rows.into_iter().map(|row| row.values).collect(),
            next_cursor: page.next_cursor,
        });
    }
    let raw = attricat::page(
        &profile.blueprint_id,
        profile.blueprint_version,
        &profile.context_id,
        cursor,
        limit,
    )
    .map_err(|error| retry(&error))?;
    parse(&raw, "invalid host page")
}

pub fn profile_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    let input = input(request)?;
    let profile = &input.profile;
    let mut s: State = state(request)?;
    let scope = api::call("connector.scope.v1", "{}").map_err(|error| retry(&error))?;
    let scoped = parse::<Value>(&scope, "invalid host scope")?.is_object();
    let header: Vec<&str> = profile.columns.iter().map(|c| c.header.as_str()).collect();
    let mut limit = PAGE_ROWS;
    let (page, bytes) = loop {
        let page = read_page(profile, scoped, &s.cursor, limit)?;
        if page.rows.len() > limit as usize {
            return Err(rejected("invalid_response", "host page exceeds limit"));
        }
        let rows: Vec<Vec<String>> = page.rows.iter().map(|r| export_row(profile, r)).collect();
        let bytes = encode_rows(
            (!s.header).then_some(&header[..]),
            profile.bom && !s.header,
            &rows,
        )
        .map_err(|error| rejected("invalid_csv", &error.to_string()))?;
        if bytes.len() <= MAX_CHUNK_BYTES {
            break (page, bytes);
        }
        if limit == 1 {
            return Err(rejected("row_too_large", "one CSV row exceeds 64 KiB"));
        }
        limit /= 2;
    };
    append(request, &bytes)?;
    s.header = true;
    s.rows += page.rows.len() as u64;
    s.cursor = page.next_cursor.unwrap_or_default();
    Ok(BatchOutcome {
        checkpoint: checkpoint(&s)?,
        progress: progress(s.rows, s.rows, 0, None),
        done: s.cursor.is_empty(),
        resume_after_ms: None,
    })
}

#[derive(Deserialize)]
struct Member {
    status: String,
    #[serde(default)]
    values: Map<String, Value>,
}
#[derive(Deserialize)]
struct Members {
    records: Vec<Member>,
    next_cursor: Option<String>,
}
fn read_members(cursor: &str, limit: u32) -> Result<Members, Failure> {
    let raw = selection::page(cursor, limit).map_err(|error| retry(&error))?;
    parse(&raw, "invalid host selection page")
}
fn selection_total() -> Result<u64, Failure> {
    let raw = selection::describe().map_err(|error| retry(&error))?;
    let described: Value = parse(&raw, "invalid host selection")?;
    Ok(described["count"].as_u64().unwrap_or_default())
}

/// The first batch reads the selection for its columns, in code order, and
/// writes the header. A selection run has no schema to take them from.
fn start_selection(request: &OperationRequest, s: &mut State) -> Result<(), Failure> {
    let mut columns = BTreeSet::new();
    let mut cursor = String::new();
    loop {
        let page = read_members(&cursor, SELECTION_ROWS)?;
        for member in page.records {
            columns.extend(member.values.keys().cloned());
        }
        match page.next_cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }
    s.columns = columns.into_iter().collect();
    if !s.columns.is_empty() {
        // Spreadsheets need the mark to read a file as UTF-8.
        let bytes = encode_rows(Some(&s.columns[..]), true, &[])
            .map_err(|error| rejected("invalid_csv", &error.to_string()))?;
        if bytes.len() > MAX_CHUNK_BYTES {
            return Err(rejected("row_too_large", "the header row exceeds 64 KiB"));
        }
        append(request, &bytes)?;
        s.header = true;
    }
    Ok(())
}

pub fn selection_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    let mut s: State = state(request)?;
    let total = selection_total()?;
    let done = if !s.header {
        start_selection(request, &mut s)?;
        s.columns.is_empty()
    } else {
        let mut limit = SELECTION_ROWS;
        loop {
            let page = read_members(&s.cursor, limit)?;
            let available = page.records.iter().filter(|m| m.status == "available");
            let rows: Vec<Vec<String>> = available
                .map(|member| {
                    s.columns
                        .iter()
                        .map(|code| selection_cell(member.values.get(code).unwrap_or(&Value::Null)))
                        .collect()
                })
                .collect();
            let bytes = encode_rows::<&str>(None, false, &rows)
                .map_err(|error| rejected("invalid_csv", &error.to_string()))?;
            if bytes.len() > MAX_CHUNK_BYTES {
                if limit > 1 {
                    limit /= 2;
                    continue;
                }
                // A record too large for one output chunk counts as failed.
                s.failed += 1;
            } else {
                append(request, &bytes)?;
                s.rows += rows.len() as u64;
                s.failed += page.records.len() as u64 - rows.len() as u64;
            }
            s.cursor = page.next_cursor.clone().unwrap_or_default();
            break page.next_cursor.is_none();
        }
    };
    Ok(BatchOutcome {
        checkpoint: checkpoint(&s)?,
        progress: progress(s.rows + s.failed, s.rows, s.failed, Some(total)),
        done,
        resume_after_ms: None,
    })
}

pub fn finish(request: &OperationRequest) -> Result<(), String> {
    let s: State = serde_json::from_str(&request.checkpoint).unwrap_or_default();
    if s.header {
        artifacts::finalize_output(OUTPUT)?;
    }
    Ok(())
}
