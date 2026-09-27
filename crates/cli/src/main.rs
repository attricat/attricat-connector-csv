//! Local profile registry and API request generator. It never stores tokens or
//! directly accesses the Catalog; the operator sends its JSON via their CLI.
use attricat_csv_core::{ExportProfile, ImportProfile};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

fn usage() -> String {
    "usage: attricat-csv-cli save <dir> <import|export> <name> <profile.json> | request <dir> <import|export> <name> <version> <idempotency-key> [file-id|-] [interval-seconds|-] [endpoint.json|-]".into()
}
fn name(s: &str) -> Result<&str, String> {
    if s.is_empty()
        || s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("profile name must be 1..64 ASCII letters/digits/dashes/underscores".into());
    }
    Ok(s)
}
fn operation(s: &str) -> Result<&str, String> {
    if matches!(s, "import" | "export") {
        Ok(s)
    } else {
        Err(usage())
    }
}
fn profile(value: &Value, operation: &str) -> Result<u64, String> {
    match operation {
        "import" => {
            let p: ImportProfile =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            p.validate().map_err(|e| e.to_string())?;
            Ok(p.version)
        }
        _ => {
            let p: ExportProfile =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            p.validate().map_err(|e| e.to_string())?;
            Ok(p.version)
        }
    }
}
fn path(dir: &Path, operation: &str, id: &str, version: u64) -> PathBuf {
    dir.join(format!("{operation}-{id}-v{version}.json"))
}
fn save(dir: &Path, op: &str, id: &str, file: &Path) -> Result<(), String> {
    let value: Value = serde_json::from_slice(&fs::read(file).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let version = profile(&value, op)?;
    let dest = path(dir, op, id, version);
    let contents = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // create_new ensures that an in-flight run never resolves a changed version.
    let mut handle = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
        .map_err(|e| e.to_string())?;
    if let Err(error) = handle.write_all(&contents).and_then(|_| handle.sync_all()) {
        let _ = fs::remove_file(&dest);
        return Err(error.to_string());
    }
    eprintln!(
        "saved {} (sha256 {})",
        dest.display(),
        hex::encode(Sha256::digest(&contents))
    );
    Ok(())
}
fn request(
    dir: &Path,
    op: &str,
    id: &str,
    version: u64,
    key: &str,
    file: &str,
    interval: &str,
    endpoint: &str,
) -> Result<Value, String> {
    if key.is_empty() || key.len() > 128 {
        return Err("idempotency key must be 1..128 bytes".into());
    }
    let value: Value =
        serde_json::from_slice(&fs::read(path(dir, op, id, version)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if profile(&value, op)? != version {
        return Err("profile version mismatch".into());
    }
    let mut input = json!({"profile":value});
    let mut source = json!({});
    if file != "-" {
        if op != "import" || file.is_empty() {
            return Err("file ID only valid for import".into());
        }
        source = json!({"input_file_id":file});
    }
    if endpoint != "-" {
        let raw: Value = serde_json::from_slice(&fs::read(endpoint).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if !raw.is_object() {
            return Err("endpoint must be an object".into());
        }
        input[if op == "import" {
            "source"
        } else {
            "destination"
        }] = raw;
        if file != "-" {
            return Err("select one source: file or HTTPS".into());
        }
    }
    if interval == "-" {
        Ok(json!({"operation_id":op,"input":input,"source_reference":source,"idempotency_key":key}))
    } else {
        let seconds: u32 = interval.parse().map_err(|_| "invalid interval seconds")?;
        if !(60..=2592000).contains(&seconds) {
            return Err("interval must be 60..2592000 seconds".into());
        }
        Ok(
            json!({"operation_id":op,"input":input,"source_reference":source,"interval_seconds":seconds}),
        )
    }
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(2);
    }
}
fn run() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [cmd, dir, op, id, file] if cmd == "save" => {
            save(Path::new(dir), operation(op)?, name(id)?, Path::new(file))
        }
        [cmd, dir, op, id, version, key, rest @ ..] if cmd == "request" && rest.len() <= 3 => {
            let version: u64 = version.parse().map_err(|_| "invalid version")?;
            let req = request(
                Path::new(dir),
                operation(op)?,
                name(id)?,
                version,
                key,
                rest.first().map(String::as_str).unwrap_or("-"),
                rest.get(1).map(String::as_str).unwrap_or("-"),
                rest.get(2).map(String::as_str).unwrap_or("-"),
            )?;
            println!("{}", req);
            Ok(())
        }
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_names_and_interval() {
        assert!(name("../../x").is_err());
        assert!(operation("delete").is_err());
        let dir = env::temp_dir().join(format!("csv-profile-{}", std::process::id()));
        let profile = json!({"version":1,"blueprint_id":"bp","blueprint_version":1,"context_id":"ctx","columns":[{"header":"ID","attribute":"id","kind":"string"}]});
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("input.json"), profile.to_string()).unwrap();
        save(&dir, "export", "example", &dir.join("input.json")).unwrap();
        assert!(save(&dir, "export", "example", &dir.join("input.json")).is_err());
        assert!(request(&dir, "export", "example", 1, "run", "-", "30", "-").is_err());
        assert_eq!(
            request(&dir, "export", "example", 1, "run", "-", "-", "-").unwrap()["operation_id"],
            "export"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
