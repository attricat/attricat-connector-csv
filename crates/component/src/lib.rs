//! Server operation component. All I/O is mediated by the host WIT imports.
wit_bindgen::generate!({ path: "wit", world: "operation-extension" });
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
    #[serde(default)]
    source: Option<HttpEndpoint>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportInput {
    profile: ExportProfile,
    #[serde(default)]
    destination: Option<HttpDestination>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpEndpoint {
    host_permission_id: String,
    url: String,
    #[serde(default)]
    secret_headers: Vec<SecretHeader>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SecretHeader {
    secret: String,
    header: String,
    #[serde(default)]
    prefix: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpDestination {
    #[serde(flatten)]
    endpoint: HttpEndpoint,
    method: String,
}
fn check_endpoint(e: &HttpEndpoint) -> Result<(), String> {
    if !e.url.starts_with("https://")
        || e.url.contains(['?', '#', '@'])
        || e.host_permission_id.is_empty()
        || e.secret_headers.len() > 8
    {
        return Err("invalid HTTPS endpoint".into());
    }
    Ok(())
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

// Range artifacts are pinned by the host. A restarted batch re-opens the same
// run/offset transfer key, never an unversioned URL. Only strong ETags permit
// continuation into a second range.
struct Source {
    handle: catalog::host::artifacts::InputArtifact,
    http: Option<(HttpEndpoint, String, u64, u64)>, // endpoint, ETag, next offset, range end
}
impl Source {
    fn new(endpoint: Option<HttpEndpoint>) -> Result<Self, String> {
        if let Some(endpoint) = endpoint {
            check_endpoint(&endpoint)?;
            let reply = fetch(&endpoint, 0, None)?;
            let etag = reply["etag"].as_str().unwrap_or("").to_owned();
            let id = reply["artifact_id"]
                .as_str()
                .ok_or("missing transfer artifact")?;
            let handle = catalog::host::artifacts::open_input(id)?;
            let length = catalog::host::artifacts::describe_input(&handle)?.content_length;
            Ok(Self {
                handle,
                http: Some((
                    endpoint,
                    etag,
                    length,
                    if length < RANGE as u64 {
                        u64::MAX
                    } else {
                        length
                    },
                )),
            })
        } else {
            Ok(Self {
                handle: catalog::host::artifacts::open_input("source")?,
                http: None,
            })
        }
    }
}
const RANGE: u32 = 16 * 1024 * 1024;
fn fetch(e: &HttpEndpoint, offset: u64, etag: Option<&str>) -> Result<Value, String> {
    let request = json!({"host_permission_id":e.host_permission_id,"url":e.url,
        "transfer_key":format!("csv-source-{offset}"),"offset":offset,"max_bytes":RANGE,
        "etag":etag,"secret_headers":e.secret_headers})
    .to_string();
    let raw = catalog::host::transfer::fetch_input(&request)?;
    serde_json::from_str(&raw).map_err(|_| "invalid transfer response".into())
}
impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut bytes = catalog::host::artifacts::read(&self.handle, out.len().min(65536) as u32)
            .map_err(std::io::Error::other)?;
        if bytes.is_empty() {
            let Some((endpoint, etag, offset, end)) = self.http.as_mut() else {
                return Ok(0);
            };
            if *offset != *end || *offset == 0 {
                return Ok(0);
            }
            if !etag.starts_with('"') || etag.starts_with("W/") {
                return Err(std::io::Error::other(
                    "range continuation requires a strong ETag",
                ));
            }
            let reply = match fetch(endpoint, *offset, Some(etag)) {
                Ok(reply) => reply,
                // A 416 at the exact end of a pinned source terminates input.
                Err(e) if e.contains("HTTP 416") => return Ok(0),
                Err(e) => return Err(std::io::Error::other(e)),
            };
            let id = reply["artifact_id"]
                .as_str()
                .ok_or_else(|| std::io::Error::other("missing transfer artifact"))?;
            self.handle =
                catalog::host::artifacts::open_input(id).map_err(std::io::Error::other)?;
            let length = catalog::host::artifacts::describe_input(&self.handle)
                .map_err(std::io::Error::other)?
                .content_length;
            *offset += length;
            *end = if length < RANGE as u64 {
                u64::MAX
            } else {
                *offset
            };
            bytes = catalog::host::artifacts::read(&self.handle, out.len().min(65536) as u32)
                .map_err(std::io::Error::other)?;
        }
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
            if let Some(ref source) = input.source {
                check_endpoint(source)?;
            }
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
            if let Some(ref destination) = input.destination {
                check_endpoint(&destination.endpoint)?;
                if !matches!(destination.method.as_str(), "POST" | "PUT") {
                    return Err("delivery requires POST or PUT".into());
                }
            }
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
    let source = Source::new(input.source.clone())?;
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
    let mut limit = 16;
    let (page, bytes) = loop {
        let raw = catalog::host::catalog::page(
            &input.profile.blueprint_id,
            input.profile.blueprint_version,
            &input.profile.context_id,
            &s.cursor,
            limit,
        )?;
        let page: Page = serde_json::from_str(&raw).map_err(|_| "invalid host page")?;
        if page.rows.len() > limit as usize {
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
        if bytes.len() <= 65536 {
            break (page, bytes);
        }
        if limit == 1 {
            return Err("one CSV row exceeds 64 KiB".into());
        }
        limit /= 2;
    };
    catalog::host::artifacts::append_output("export.csv", "text/csv", &request.batch_key, &bytes)?;
    s.header = true;
    s.rows += page.rows.len() as u64;
    s.cursor = page.next_cursor.unwrap_or_default();
    batch(&s, s.cursor.is_empty())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_does_not_accept_insecure_or_credentialed_urls() {
        let mut endpoint = HttpEndpoint {
            host_permission_id: "csv-source".into(),
            url: "https://example.org/import/file.csv".into(),
            secret_headers: vec![],
        };
        assert!(check_endpoint(&endpoint).is_ok());
        endpoint.url = "http://example.org/import/file.csv".into();
        assert!(check_endpoint(&endpoint).is_err());
        endpoint.url = "https://user@example.org/import/file.csv".into();
        assert!(check_endpoint(&endpoint).is_err());
        endpoint.url = "https://example.org/import/file.csv?token=value".into();
        assert!(check_endpoint(&endpoint).is_err());
    }
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
                let artifact_id = catalog::host::artifacts::finalize_output("export.csv")?;
                let input: ExportInput =
                    serde_json::from_str(&request.input).map_err(|_| "invalid export input")?;
                if let Some(destination) = input.destination {
                    let body = json!({"host_permission_id":destination.endpoint.host_permission_id,
                        "url":destination.endpoint.url,"method":destination.method,"artifact_id":artifact_id,
                        "delivery_key":"csv-export-v1","secret_headers":destination.endpoint.secret_headers}).to_string();
                    // The host records an uncertain attempt before network I/O. A
                    // finish replay reads its recorded outcome without resending.
                    let response = catalog::host::transfer::deliver_output(&body)?;
                    let outcome: Value =
                        serde_json::from_str(&response).map_err(|_| "invalid delivery response")?;
                    if !matches!(
                        outcome["outcome"].as_str(),
                        Some("succeeded" | "failed" | "uncertain")
                    ) {
                        return Err("invalid delivery outcome".into());
                    }
                }
            }
            _ => return Err("unknown operation".into()),
        }
        Ok(())
    }
    fn cancel(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }
}
