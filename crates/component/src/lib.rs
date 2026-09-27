//! Server operation component. All I/O is mediated by the host WIT imports.
wit_bindgen::generate!({ path: "wit", world: "catalog-extension-operation" });
use attricat_csv_core::{ExportProfile, ImportProfile, ImportRows, export_row};
use exports::catalog::host::operations::{BatchResult, Guest, OperationRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;

struct Component;
export!(Component);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportInput {
    profile: ImportProfile,
    #[serde(default)]
    dry_run: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportInput {
    profile: ExportProfile,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct State {
    rows: u64,
    rejected: u64,
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    header: bool,
}

struct Source(catalog::host::artifacts::InputArtifact);
impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let bytes = catalog::host::artifacts::read(&self.0, out.len().min(65536) as u32)
            .map_err(std::io::Error::other)?;
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

// Host schema response: {"attributes":[{"id":"...","kind":"string"},...]}
// This validation is only a preflight; the host must validate again at every
// read/mutation, including on replay and after permission loss.
fn check_schema(
    blueprint: &str,
    version: u64,
    context: &str,
    columns: &[attricat_csv_core::Column],
) -> Result<(), String> {
    let raw = catalog::host::catalog::schema(blueprint, version, context)?;
    let schema: Value = serde_json::from_str(&raw).map_err(|_| "invalid host schema")?;
    let attrs = schema["attributes"]
        .as_array()
        .ok_or("invalid host attributes")?;
    for col in columns {
        let expected = match col.kind {
            attricat_csv_core::Kind::String => "string",
            attricat_csv_core::Kind::Integer => "integer",
            attricat_csv_core::Kind::Number => "number",
            attricat_csv_core::Kind::Boolean => "boolean",
        };
        if !attrs
            .iter()
            .any(|a| a["id"] == col.attribute && a["kind"] == expected)
        {
            return Err(format!(
                "attribute not found or incompatible: {}",
                col.attribute
            ));
        }
    }
    Ok(())
}
fn state(request: &OperationRequest) -> Result<State, String> {
    if request.checkpoint.is_empty() {
        Ok(State::default())
    } else {
        serde_json::from_str(&request.checkpoint).map_err(|_| "invalid checkpoint".into())
    }
}
fn batch(s: &State, done: bool) -> Result<BatchResult, String> {
    Ok(BatchResult {
        checkpoint: serde_json::to_string(s).map_err(|e| e.to_string())?,
        progress: json!({"rows":s.rows,"rejected":s.rejected}).to_string(),
        done,
    })
}
fn validate(request: &OperationRequest) -> Result<(), String> {
    match request.operation_id.as_str() {
        "import" => {
            let input: ImportInput =
                serde_json::from_str(&request.input).map_err(|_| "invalid import input")?;
            input.profile.validate().map_err(|e| e.to_string())?;
            check_schema(
                &input.profile.blueprint_id,
                input.profile.blueprint_version,
                &input.profile.context_id,
                &input.profile.columns,
            )
        }
        "export" => {
            let input: ExportInput =
                serde_json::from_str(&request.input).map_err(|_| "invalid export input")?;
            input.profile.validate().map_err(|e| e.to_string())?;
            check_schema(
                &input.profile.blueprint_id,
                input.profile.blueprint_version,
                &input.profile.context_id,
                &input.profile.columns,
            )
        }
        _ => Err("unknown operation".into()),
    }
}
fn import(request: OperationRequest) -> Result<BatchResult, String> {
    let input: ImportInput =
        serde_json::from_str(&request.input).map_err(|_| "invalid import input")?;
    input.profile.validate().map_err(|e| e.to_string())?;
    let mut s = state(&request)?;
    let source = Source(catalog::host::artifacts::open_input("source")?);
    let mut rows = ImportRows::new(source, input.profile.clone()).map_err(|e| e.to_string())?;
    // Re-reading a pinned source from the start avoids persisting an unsafe
    // byte offset into a quoted record. Memory stays bounded across retries.
    let mut seen = HashSet::new();
    for _ in 0..s.rows {
        match rows.next_row() {
            Ok(Some(row)) => {
                seen.insert(row.key.to_string());
            }
            Err(attricat_csv_core::Error::Row { .. }) => {}
            _ => return Err("source changed or checkpoint exceeds source".into()),
        }
    }
    let mut intents = Vec::new();
    let mut errors = Vec::new();
    let mut done = false;
    for _ in 0..16 {
        match rows.next_row() {
            Ok(Some(row)) => {
                s.rows += 1;
                if !seen.insert(row.key.to_string()) {
                    s.rejected += 1;
                    errors.push(json!({"row":row.row,"error":"duplicate business key"}));
                } else {
                    intents.push(json!({"row":row.row,"business_key":input.profile.business_key,"key":row.key,"values":row.values}));
                }
            }
            Err(attricat_csv_core::Error::Row { row, message }) => {
                s.rows += 1;
                s.rejected += 1;
                errors.push(json!({"row":row,"error":message}));
            }
            Err(e) => return Err(e.to_string()),
            Ok(None) => {
                done = true;
                break;
            }
        }
    }
    // Never write a batch until every candidate row has been checked. Stable
    // run/batch keys make host batch deduplication mandatory, not optional.
    if !input.dry_run && !intents.is_empty() {
        let body = json!({"blueprint_id":input.profile.blueprint_id,"blueprint_version":input.profile.blueprint_version,"context_id":input.profile.context_id,"run_id":request.run_id,"batch_key":request.batch_key,"intents":intents}).to_string();
        if body.len() > 65536 {
            return Err("import batch exceeds 64 KiB".into());
        }
        catalog::host::catalog::upsert_batch(&body)?;
    }
    if !errors.is_empty() {
        let data = errors
            .into_iter()
            .map(|e| format!("{e}\n"))
            .collect::<String>();
        catalog::host::artifacts::append_output(
            "rejections.ndjson",
            "application/x-ndjson",
            &request.batch_key,
            data.as_bytes(),
        )?;
    }
    batch(&s, done)
}

#[derive(Deserialize)]
struct Page {
    rows: Vec<HashMap<String, Value>>,
    next_cursor: Option<String>,
}
fn export_batch(request: OperationRequest) -> Result<BatchResult, String> {
    let input: ExportInput =
        serde_json::from_str(&request.input).map_err(|_| "invalid export input")?;
    input.profile.validate().map_err(|e| e.to_string())?;
    let mut s = state(&request)?;
    let raw = catalog::host::catalog::page(
        &input.profile.blueprint_id,
        input.profile.blueprint_version,
        &input.profile.context_id,
        &s.cursor,
        16,
    )?;
    let page: Page = serde_json::from_str(&raw).map_err(|_| "invalid host page")?;
    if page.rows.len() > 16 {
        return Err("host page exceeds limit".into());
    }
    let mut writer = csv::Writer::from_writer(Vec::new());
    if !s.header {
        writer
            .write_record(input.profile.columns.iter().map(|c| &c.header))
            .map_err(|e| e.to_string())?;
    }
    for row in &page.rows {
        writer
            .write_record(export_row(&input.profile, row))
            .map_err(|e| e.to_string())?;
    }
    let bytes = writer.into_inner().map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("export batch exceeds 64 KiB; reduce page size".into());
    }
    catalog::host::artifacts::append_output("export.csv", "text/csv", &request.batch_key, &bytes)?;
    s.header = true;
    s.rows += page.rows.len() as u64;
    s.cursor = page.next_cursor.unwrap_or_default();
    batch(&s, s.cursor.is_empty())
}
impl Guest for Component {
    fn prepare(request: OperationRequest) -> Result<String, String> {
        validate(&request)?;
        Ok("{}".into())
    }
    fn start(request: OperationRequest) -> Result<String, String> {
        validate(&request)?;
        Ok("{}".into())
    }
    fn process_batch(request: OperationRequest) -> Result<BatchResult, String> {
        validate(&request)?;
        match request.operation_id.as_str() {
            "import" => import(request),
            "export" => export_batch(request),
            _ => Err("unknown operation".into()),
        }
    }
    fn checkpoint(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }
    fn finish(request: OperationRequest) -> Result<(), String> {
        match request.operation_id.as_str() {
            "import" => {
                let s = state(&request)?;
                if s.rejected > 0 {
                    catalog::host::artifacts::finalize_output("rejections.ndjson")?;
                }
            }
            "export" => {
                catalog::host::artifacts::finalize_output("export.csv")?;
            }
            _ => return Err("unknown operation".into()),
        }
        Ok(())
    }
    fn cancel(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }
}
