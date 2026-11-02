//! Server operation component. All I/O is mediated by the host WIT imports.
wit_bindgen::generate!({ path: "wit", world: "outcome-extension" });
use attricat::host::outcomes::{Failure, Rejection, Retry};
use exports::attricat::host::deferrable_operations::{BatchOutcome, Guest, OperationRequest};
use exports::attricat::host::{command_handler, event_handler};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

mod export;
mod import;

struct Component;
export!(Component);

/// Operation handlers declared in `manifest.json`.
const IMPORT: &str = "import";
const IMPORT_FILE: &str = "import-file";
const EXPORT: &str = "export";
const EXPORT_SELECTION: &str = "export-selection";

/// Run errors keep only structured JSON, so messages travel as a JSON object.
fn diagnostic(message: &str) -> String {
    json!({ "error": message }).to_string()
}
/// Ends the run with a reason a person can act on. Never quarantines.
fn rejected(code: &str, message: &str) -> Failure {
    Failure::Rejected(Rejection {
        code: code.into(),
        message: diagnostic(message),
        record_id: None,
        connection: None,
    })
}
/// Runs the batch again with the same batch key, spending the failure budget.
fn retry(message: &str) -> Failure {
    Failure::Retry(Retry {
        message: diagnostic(message),
        after_ms: None,
    })
}
fn parse<T: DeserializeOwned>(raw: &str, what: &str) -> Result<T, Failure> {
    serde_json::from_str(raw).map_err(|_| rejected("invalid_response", what))
}
fn state<T: DeserializeOwned + Default>(request: &OperationRequest) -> Result<T, Failure> {
    if request.checkpoint.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&request.checkpoint)
        .map_err(|_| rejected("invalid_checkpoint", "invalid checkpoint"))
}
fn schema(
    blueprint: &str,
    version: u64,
    context: &str,
) -> Result<Vec<attricat_csv_core::SchemaAttribute>, Failure> {
    #[derive(Deserialize)]
    struct Schema {
        attributes: Vec<attricat_csv_core::SchemaAttribute>,
    }
    let raw = attricat::host::attricat::schema(blueprint, version, context)
        .map_err(|error| retry(&error))?;
    Ok(parse::<Schema>(&raw, "invalid host schema")?.attributes)
}
fn progress(rows: u64, succeeded: u64, failed: u64, total: Option<u64>) -> String {
    let mut progress = json!({
        "completed": rows,
        "outcome": { "succeeded": succeeded, "failed": failed, "skipped": 0 },
    });
    if let Some(total) = total {
        progress["total"] = Value::from(total);
    }
    progress.to_string()
}
fn checkpoint<T: serde::Serialize>(state: &T) -> Result<String, Failure> {
    serde_json::to_string(state).map_err(|error| rejected("invalid_checkpoint", &error.to_string()))
}
/// Preflight of operations that take a profile. The host validates again at
/// every read and mutation, including on replay and after permission loss.
fn validate(request: &OperationRequest) -> Result<(), String> {
    let failed = |failure: Failure| match failure {
        Failure::Rejected(rejection) => rejection.message,
        Failure::Retry(retry) => retry.message,
        Failure::Fault(message) => message,
    };
    match request.operation_id.as_str() {
        IMPORT => import::validate(request).map_err(failed),
        EXPORT => export::validate(request).map_err(failed),
        IMPORT_FILE | EXPORT_SELECTION => Ok(()),
        _ => Err(diagnostic("unknown operation")),
    }
}

impl Guest for Component {
    fn prepare(request: OperationRequest) -> Result<String, String> {
        validate(&request)?;
        Ok("{}".into())
    }
    fn start(_request: OperationRequest) -> Result<String, String> {
        Ok("{}".into())
    }
    fn process_batch(request: OperationRequest) -> Result<BatchOutcome, Failure> {
        match request.operation_id.as_str() {
            IMPORT => import::profile_batch(&request),
            IMPORT_FILE => import::file_batch(&request),
            EXPORT => export::profile_batch(&request),
            EXPORT_SELECTION => export::selection_batch(&request),
            _ => Err(rejected("unknown_operation", "unknown operation")),
        }
    }
    fn checkpoint(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }
    fn finish(request: OperationRequest) -> Result<(), String> {
        match request.operation_id.as_str() {
            IMPORT | IMPORT_FILE => import::finish(&request),
            EXPORT | EXPORT_SELECTION => export::finish(&request),
            _ => Err(diagnostic("unknown operation")),
        }
        .map_err(|error| diagnostic(&error))
    }
    fn cancel(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }
}

// This release declares no event handlers or commands, so the host never calls these.
impl event_handler::Guest for Component {
    fn handle_event(
        _event: event_handler::Event,
        _delivery: event_handler::Delivery,
    ) -> Result<(), Failure> {
        Err(Failure::Fault("no event handlers declared".into()))
    }
}
impl command_handler::Guest for Component {
    fn handle_command(
        _request: command_handler::CommandRequest,
    ) -> Result<command_handler::CommandResponse, String> {
        Err(diagnostic("no commands declared"))
    }
}
