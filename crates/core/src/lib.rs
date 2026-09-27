//! Pure, host-independent CSV mapping. No ambient filesystem or network access.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid profile: {0}")]
    Profile(String),
    #[error("CSV: {0}")]
    Csv(#[from] csv::Error),
    #[error("row {row}: {message}")]
    Row { row: u64, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub header: String,
    pub attribute: String,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    String,
    Integer,
    Number,
    Boolean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportProfile {
    pub version: u64,
    pub blueprint_id: String,
    pub blueprint_version: u64,
    pub context_id: String,
    pub columns: Vec<Column>,
    pub business_key: String,
    #[serde(default)]
    pub unknown_headers: UnknownHeaders,
    #[serde(default)]
    pub empty: EmptyPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownHeaders {
    #[default]
    Reject,
    Ignore,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyPolicy {
    #[default]
    Null,
    EmptyString,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportProfile {
    pub version: u64,
    pub blueprint_id: String,
    pub blueprint_version: u64,
    pub context_id: String,
    pub columns: Vec<Column>,
}

fn validate_columns(columns: &[Column]) -> Result<(), Error> {
    if columns.is_empty() || columns.len() > 128 {
        return Err(Error::Profile("1..128 columns required".into()));
    }
    let (mut headers, mut attrs) = (HashSet::new(), HashSet::new());
    for col in columns {
        if col.header.is_empty()
            || col.header.len() > 256
            || col.attribute.is_empty()
            || !headers.insert(&col.header)
            || !attrs.insert(&col.attribute)
        {
            return Err(Error::Profile(
                "empty, oversized or duplicate header/attribute".into(),
            ));
        }
        if let Some(ref value) = col.default {
            convert_default(value, &col.kind)?;
        }
    }
    Ok(())
}

fn convert_default(value: &Value, kind: &Kind) -> Result<(), Error> {
    if value.is_null()
        || matches!(
            (value, kind),
            (Value::String(_), Kind::String)
                | (Value::Bool(_), Kind::Boolean)
                | (Value::Number(_), Kind::Integer | Kind::Number)
        )
    {
        if matches!(kind, Kind::Integer) && !value.is_null() && value.as_i64().is_none() {
            return Err(Error::Profile("integer default out of range".into()));
        }
        Ok(())
    } else {
        Err(Error::Profile("default type does not match column".into()))
    }
}

impl ImportProfile {
    pub fn validate(&self) -> Result<(), Error> {
        validate_identity(
            self.version,
            &self.blueprint_id,
            self.blueprint_version,
            &self.context_id,
        )?;
        validate_columns(&self.columns)?;
        if !self
            .columns
            .iter()
            .any(|c| c.attribute == self.business_key && matches!(c.kind, Kind::String))
        {
            return Err(Error::Profile(
                "business key must be a mapped string attribute".into(),
            ));
        }
        Ok(())
    }
}
impl ExportProfile {
    pub fn validate(&self) -> Result<(), Error> {
        validate_identity(
            self.version,
            &self.blueprint_id,
            self.blueprint_version,
            &self.context_id,
        )?;
        validate_columns(&self.columns)
    }
}
fn validate_identity(
    version: u64,
    blueprint: &str,
    revision: u64,
    context: &str,
) -> Result<(), Error> {
    if version == 0 || revision == 0 || blueprint.is_empty() || context.is_empty() {
        return Err(Error::Profile(
            "version, blueprint revision and identifiers required".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappedRow {
    pub row: u64,
    pub key: Value,
    pub values: HashMap<String, Value>,
}

// Bound logical record size before csv allocates a ByteRecord, tracking quoted
// newlines and doubled quotes even when a Read splits them.
struct RecordLimit<R: Read> {
    source: BufReader<R>,
    quoted: bool,
    quote_pending: bool,
    field_start: bool,
    length: usize,
}
impl<R: Read> Read for RecordLimit<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut count = 0;
        while count < out.len() {
            let available = self.source.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let byte = available[0];
            self.source.consume(1);
            self.length += 1;
            if self.length > 65536 {
                if count == 0 {
                    return Err(std::io::Error::other("CSV record exceeds 64 KiB"));
                }
                // Return preceding bytes; next call must report the limit.
                self.length = 65537;
                break;
            }
            out[count] = byte;
            count += 1;
            if self.quoted {
                if self.quote_pending {
                    if byte == b'"' {
                        self.quote_pending = false;
                        continue;
                    }
                    self.quoted = false;
                    self.quote_pending = false;
                } else if byte == b'"' {
                    self.quote_pending = true;
                    continue;
                } else {
                    continue;
                }
            }
            match byte {
                b'"' if self.field_start => {
                    self.quoted = true;
                    self.field_start = false;
                }
                b',' => self.field_start = true,
                b'\n' => {
                    self.length = 0;
                    self.field_start = true;
                }
                b'\r' => {}
                _ => self.field_start = false,
            }
        }
        Ok(count)
    }
}
/// RFC 4180 reader with a pre-allocation 64 KiB record bound.
pub struct ImportRows<R: Read> {
    reader: csv::Reader<RecordLimit<R>>,
    indices: Vec<usize>,
    profile: ImportProfile,
    row: u64,
}
impl<R: Read> ImportRows<R> {
    pub fn new(source: R, profile: ImportProfile) -> Result<Self, Error> {
        profile.validate()?;
        let mut reader = csv::ReaderBuilder::new()
            .flexible(false)
            .from_reader(RecordLimit {
                source: BufReader::new(source),
                quoted: false,
                quote_pending: false,
                field_start: true,
                length: 0,
            });
        let headers = reader.byte_headers()?.clone();
        let mut seen = HashSet::new();
        let mut positions = HashMap::new();
        for (index, raw) in headers.iter().enumerate() {
            let raw = if index == 0 {
                raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(raw)
            } else {
                raw
            };
            let name =
                std::str::from_utf8(raw).map_err(|_| Error::Profile("non-UTF-8 header".into()))?;
            if name.is_empty() || !seen.insert(name.to_string()) {
                return Err(Error::Profile("empty or duplicate CSV header".into()));
            }
            positions.insert(name.to_string(), index);
        }
        if matches!(profile.unknown_headers, UnknownHeaders::Reject)
            && positions
                .keys()
                .any(|h| !profile.columns.iter().any(|c| &c.header == h))
        {
            return Err(Error::Profile("unknown CSV header".into()));
        }
        let indices = profile
            .columns
            .iter()
            .map(|c| {
                positions
                    .get(&c.header)
                    .copied()
                    .ok_or_else(|| Error::Profile(format!("missing CSV header: {}", c.header)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            reader,
            indices,
            profile,
            row: 1,
        })
    }
    pub fn next_row(&mut self) -> Result<Option<MappedRow>, Error> {
        let mut record = csv::ByteRecord::new();
        if !self.reader.read_byte_record(&mut record)? {
            return Ok(None);
        }
        self.row += 1;
        if record.as_slice().len() > 65536 {
            return Err(Error::Row {
                row: self.row,
                message: "record exceeds 64 KiB".into(),
            });
        }
        let mut values = HashMap::new();
        for (col, &index) in self.profile.columns.iter().zip(&self.indices) {
            let raw = record.get(index).ok_or_else(|| Error::Row {
                row: self.row,
                message: "missing field".into(),
            })?;
            let text = std::str::from_utf8(raw).map_err(|_| Error::Row {
                row: self.row,
                message: "non-UTF-8 field".into(),
            })?;
            let value = if text.is_empty() {
                col.default
                    .clone()
                    .unwrap_or_else(|| match self.profile.empty {
                        EmptyPolicy::Null => Value::Null,
                        EmptyPolicy::EmptyString if matches!(col.kind, Kind::String) => {
                            Value::String(String::new())
                        }
                        EmptyPolicy::EmptyString => Value::Null,
                    })
            } else {
                parse_value(text, &col.kind).map_err(|message| Error::Row {
                    row: self.row,
                    message: format!("{}: {message}", col.header),
                })?
            };
            values.insert(col.attribute.clone(), value);
        }
        let key = values.get(&self.profile.business_key).cloned().unwrap();
        if key.is_null() || key == Value::String(String::new()) {
            return Err(Error::Row {
                row: self.row,
                message: "empty business key".into(),
            });
        }
        Ok(Some(MappedRow {
            row: self.row,
            key,
            values,
        }))
    }
}
fn parse_value(text: &str, kind: &Kind) -> Result<Value, &'static str> {
    match kind {
        Kind::String => Ok(Value::String(text.into())),
        Kind::Integer => text
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "invalid integer"),
        Kind::Number => text
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or("invalid finite number"),
        Kind::Boolean => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err("expected true or false"),
        },
    }
}

/// Formulas are prefixed with an apostrophe, including those with leading whitespace.
/// Consumers must not strip this prefix before opening in a spreadsheet.
pub fn safe_cell(text: &str) -> String {
    if text
        .trim_start()
        .starts_with(['=', '+', '-', '@', '\t', '\r'])
    {
        format!("'{text}")
    } else {
        text.into()
    }
}
pub fn export_row(profile: &ExportProfile, values: &HashMap<String, Value>) -> Vec<String> {
    profile
        .columns
        .iter()
        .map(|c| match values.get(&c.attribute).unwrap_or(&Value::Null) {
            Value::Null => String::new(),
            Value::String(s) => safe_cell(s),
            v => safe_cell(&v.to_string()),
        })
        .collect()
}
pub fn write_csv_row<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    cells: &[String],
) -> Result<(), Error> {
    writer.write_record(cells)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> ImportProfile {
        ImportProfile {
            version: 1,
            blueprint_id: "bp".into(),
            blueprint_version: 1,
            context_id: "ctx".into(),
            columns: vec![
                Column {
                    header: "id".into(),
                    attribute: "key".into(),
                    kind: Kind::String,
                    default: None,
                },
                Column {
                    header: "count".into(),
                    attribute: "count".into(),
                    kind: Kind::Integer,
                    default: Some(Value::from(0)),
                },
            ],
            business_key: "key".into(),
            unknown_headers: UnknownHeaders::Reject,
            empty: EmptyPolicy::Null,
        }
    }
    #[test]
    fn split_reads_and_quoted_newlines() {
        struct Single<'a>(&'a [u8]);
        impl Read for Single<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let mut rows = ImportRows::new(
            Single(b"\xef\xbb\xbfid,count\r\n\"A\nB\",2\r\nC,\r\n"),
            profile(),
        )
        .unwrap();
        assert_eq!(rows.next_row().unwrap().unwrap().key, "A\nB");
        assert_eq!(rows.next_row().unwrap().unwrap().values["count"], 0);
        assert!(rows.next_row().unwrap().is_none());
    }
    #[test]
    fn rejects_bad_headers_and_row() {
        assert!(ImportRows::new(&b"id,id\nx,y"[..], profile()).is_err());
        let mut r = ImportRows::new(&b"id,count\na,nope"[..], profile()).unwrap();
        assert!(matches!(r.next_row(), Err(Error::Row { row: 2, .. })));
    }
    #[test]
    fn quoted_record_bound_spans_newlines_and_escaped_quotes() {
        let csv = format!(
            "id,count\n\"{}\"\"\n{}\",1\n",
            "x".repeat(35000),
            "y".repeat(35000)
        );
        let mut rows = ImportRows::new(csv.as_bytes(), profile()).unwrap();
        assert!(rows.next_row().is_err());
        let mut rows = ImportRows::new(&b"id,count\na\"b,2\nc,3\n"[..], profile()).unwrap();
        assert_eq!(rows.next_row().unwrap().unwrap().key, "a\"b");
        assert_eq!(rows.next_row().unwrap().unwrap().key, "c");
    }
    #[test]
    fn oversized_record_is_rejected_before_allocation() {
        let csv = format!("id,count\n{},1\n", "x".repeat(70000));
        let mut rows = ImportRows::new(csv.as_bytes(), profile()).unwrap();
        assert!(rows.next_row().is_err());
    }
    #[test]
    fn numeric_business_key_is_rejected() {
        let mut p = profile();
        p.business_key = "count".into();
        assert!(p.validate().is_err());
    }
    #[test]
    fn policy_and_defaults() {
        let mut p = profile();
        p.columns[1].default = None;
        p.empty = EmptyPolicy::EmptyString;
        let mut r = ImportRows::new(&b"id,count\na,\nb,4\n"[..], p).unwrap();
        assert_eq!(r.next_row().unwrap().unwrap().values["count"], Value::Null);
        assert_eq!(r.next_row().unwrap().unwrap().values["count"], 4);
        assert!(r.next_row().unwrap().is_none());
        let mut invalid = profile();
        invalid.columns[1].default = Some(Value::String("wrong".into()));
        assert!(invalid.validate().is_err());
        assert!(ImportRows::new(&b"id,count,unexpected\na,2,x"[..], profile()).is_err());
    }
    #[test]
    fn formula_and_quotes() {
        let mut w = csv::Writer::from_writer(Vec::new());
        write_csv_row(&mut w, &[safe_cell(" =1+1"), "a,\"b\n".into()]).unwrap();
        let output = String::from_utf8(w.into_inner().unwrap()).unwrap();
        assert_eq!(output, "' =1+1,\"a,\"\"b\n\"\n");
    }
}
