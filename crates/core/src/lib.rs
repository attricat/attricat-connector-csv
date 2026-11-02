//! Pure, host-independent CSV mapping. No ambient filesystem or network access.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};

/// The largest logical CSV record, and the size of one host read.
pub const MAX_RECORD_BYTES: usize = 65536;
const MAX_COLUMNS: usize = 128;
const MAX_REPORT_MESSAGE_BYTES: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid profile: {0}")]
    Profile(String),
    #[error("{0}")]
    Header(String),
    #[error("CSV: {0}")]
    Csv(#[from] csv::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    String,
    Integer,
    Number,
    Boolean,
    Date,
    Datetime,
    Time,
    Json,
}

impl Kind {
    /// The attribute `value_type` this kind writes.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::String => "string",
            Kind::Integer => "integer",
            Kind::Number => "number",
            Kind::Boolean => "boolean",
            Kind::Date => "date",
            Kind::Datetime => "datetime",
            Kind::Time => "time",
            Kind::Json => "json",
        }
    }
    fn from_value_type(value_type: &str) -> Option<Self> {
        [
            Kind::String,
            Kind::Integer,
            Kind::Number,
            Kind::Boolean,
            Kind::Date,
            Kind::Datetime,
            Kind::Time,
            Kind::Json,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == value_type)
    }
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownHeaders {
    #[default]
    Reject,
    Ignore,
}

/// What an empty cell without a column default writes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyPolicy {
    /// Leaves the attribute as it is.
    #[default]
    Skip,
    /// Writes null.
    Null,
    /// Writes an empty string to string columns, and leaves other columns.
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
    /// Starts the file with a UTF-8 byte order mark, for spreadsheets.
    #[serde(default)]
    pub bom: bool,
}

fn validate_columns(columns: &[Column]) -> Result<(), Error> {
    if columns.is_empty() || columns.len() > MAX_COLUMNS {
        return Err(Error::Profile(format!("1..{MAX_COLUMNS} columns required")));
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
            check_default(value, col.kind)?;
        }
    }
    Ok(())
}

fn check_default(value: &Value, kind: Kind) -> Result<(), Error> {
    let matches = match kind {
        Kind::Json => true,
        Kind::String | Kind::Date | Kind::Datetime | Kind::Time => {
            value.is_null() || value.is_string()
        }
        Kind::Boolean => value.is_null() || value.is_boolean(),
        Kind::Number => value.is_null() || value.is_number(),
        Kind::Integer => value.is_null() || value.as_i64().is_some(),
    };
    if matches {
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
            .any(|c| c.attribute == self.business_key && c.kind == Kind::String)
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

/// One attribute of the host's `attricat.schema` answer.
#[derive(Debug, Clone, Deserialize)]
pub struct SchemaAttribute {
    pub id: String,
    pub kind: String,
}

/// Checks that every column names an attribute of the column's kind.
pub fn check_schema(columns: &[Column], schema: &[SchemaAttribute]) -> Result<(), Error> {
    for col in columns {
        if !schema
            .iter()
            .any(|a| a.id == col.attribute && a.kind == col.kind.as_str())
        {
            return Err(Error::Profile(format!(
                "attribute not found or incompatible: {}",
                col.attribute
            )));
        }
    }
    Ok(())
}

/// Maps a file's headers to attributes by code, ignoring case and surrounding
/// spaces. A column that fits no attribute is an error rather than dropped data.
pub fn columns_from_headers(
    headers: &[String],
    schema: &[SchemaAttribute],
) -> Result<Vec<Column>, Error> {
    let mut columns = Vec::with_capacity(headers.len());
    let mut unknown = Vec::new();
    let mut unsupported = Vec::new();
    for header in headers {
        let name = header.trim();
        let found = schema
            .iter()
            .find(|a| a.id == name)
            .or_else(|| schema.iter().find(|a| a.id.eq_ignore_ascii_case(name)));
        match found {
            None => unknown.push(name),
            Some(attribute) => match Kind::from_value_type(&attribute.kind) {
                None => unsupported.push(name),
                Some(kind) => columns.push(Column {
                    header: header.clone(),
                    attribute: attribute.id.clone(),
                    kind,
                    default: None,
                }),
            },
        }
    }
    if !unknown.is_empty() {
        return Err(Error::Header(format!(
            "These columns match no attribute of this blueprint: {}. Name each column after an attribute code, or remove it.",
            name_list(&unknown)
        )));
    }
    if !unsupported.is_empty() {
        return Err(Error::Header(format!(
            "CSV import cannot write relationship or file attributes. Remove these columns: {}.",
            name_list(&unsupported)
        )));
    }
    validate_columns(&columns).map_err(|_| {
        Error::Header(format!(
            "A file needs 1 to {MAX_COLUMNS} columns, each for a different attribute."
        ))
    })?;
    Ok(columns)
}

fn name_list(names: &[&str]) -> String {
    let shown = names
        .iter()
        .take(10)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    match names.len() {
        0..=10 => shown,
        more => format!("{shown} and {} more", more - 10),
    }
}

/// How the cells of a file become attribute values.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub columns: Vec<Column>,
    /// The attribute that identifies a record; without one every row creates.
    pub business_key: Option<String>,
    pub unknown_headers: UnknownHeaders,
    pub empty: EmptyPolicy,
}

impl From<&ImportProfile> for Mapping {
    fn from(profile: &ImportProfile) -> Self {
        Self {
            columns: profile.columns.clone(),
            business_key: Some(profile.business_key.clone()),
            unknown_headers: profile.unknown_headers,
            empty: profile.empty,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappedRow {
    pub row: u64,
    pub key: Option<String>,
    pub values: BTreeMap<String, Value>,
}

// Bound logical record size before csv allocates a ByteRecord, tracking quoted
// newlines and doubled quotes even when a Read splits them.
struct RecordLimit<R: Read> {
    source: BufReader<R>,
    delimiter: u8,
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
            if self.length > MAX_RECORD_BYTES {
                if count == 0 {
                    return Err(std::io::Error::other(
                        "a CSV record exceeds 64 KiB, or a quote is never closed",
                    ));
                }
                // Return preceding bytes; next call must report the limit.
                self.length = MAX_RECORD_BYTES + 1;
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
                b'\n' | b'\r' => {
                    self.length = 0;
                    self.field_start = true;
                }
                byte if byte == self.delimiter => self.field_start = true,
                _ => self.field_start = false,
            }
        }
        Ok(count)
    }
}

/// Where to continue a file, as saved in a run's checkpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// The byte after the last record read; 0 before the first record.
    pub offset: u64,
    /// The file row of the last record read, counting the header as row 1.
    pub row: u64,
}

/// RFC 4180 reader with a pre-allocation 64 KiB record bound. Comma, semicolon
/// and tab files are told apart by their header.
pub struct CsvReader<R: Read> {
    reader: csv::Reader<RecordLimit<R>>,
    headers: Vec<String>,
    delimiter: u8,
    base: u64,
    row: u64,
}
impl<R: Read> CsvReader<R> {
    /// Reads the header, then continues after `position` if it has one. The
    /// host cannot seek an input, so continuing reads and drops what precedes.
    pub fn open(source: R, position: Position) -> Result<Self, Error> {
        let mut source = BufReader::with_capacity(MAX_RECORD_BYTES, source);
        let (raw, consumed) = read_header(&mut source)?;
        let delimiter = detect_delimiter(&raw);
        let headers = parse_header(&raw, delimiter)?;
        let base = position.offset.max(consumed);
        let mut remaining = base - consumed;
        while remaining > 0 {
            let available = source.fill_buf()?;
            if available.is_empty() {
                return Err(Error::Header(
                    "The file ended before the position this run had reached.".into(),
                ));
            }
            let step = available
                .len()
                .min(remaining.min(usize::MAX as u64) as usize);
            source.consume(step);
            remaining -= step as u64;
        }
        let reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .delimiter(delimiter)
            .from_reader(RecordLimit {
                source,
                delimiter,
                quoted: false,
                quote_pending: false,
                field_start: true,
                length: 0,
            });
        Ok(Self {
            reader,
            headers,
            delimiter,
            base,
            row: position.row.max(1),
        })
    }
    pub fn headers(&self) -> &[String] {
        &self.headers
    }
    pub fn position(&self) -> Position {
        Position {
            offset: self.base + self.reader.position().byte(),
            row: self.row,
        }
    }
    /// The next record with any cell in it; rows of empty cells are passed over.
    pub fn next_record(&mut self) -> Result<Option<csv::ByteRecord>, Error> {
        let mut record = csv::ByteRecord::new();
        loop {
            if !self.reader.read_byte_record(&mut record)? {
                return Ok(None);
            }
            self.row += 1;
            if record.iter().any(|field| !field.is_empty()) {
                return Ok(Some(record));
            }
        }
    }
    /// The row of the record `next_record` returned last.
    pub fn row(&self) -> u64 {
        self.row
    }
    /// Maps the record `next_record` returned last.
    pub fn map(&self, plan: &Plan, record: &csv::ByteRecord) -> Result<MappedRow, Error> {
        let row = self.row;
        let reject = |message: String| Error::Row { row, message };
        if record.len() != self.headers.len() {
            return Err(reject(format!(
                "expected {} fields, found {}",
                self.headers.len(),
                record.len()
            )));
        }
        let mut values = BTreeMap::new();
        for (col, &index) in plan.mapping.columns.iter().zip(&plan.indices) {
            let raw = record.get(index).unwrap_or_default();
            let text = std::str::from_utf8(raw)
                .map_err(|_| reject(format!("{}: not valid UTF-8", col.header)))?;
            let value = if text.is_empty() {
                match (&col.default, plan.mapping.empty, col.kind) {
                    (Some(default), ..) => default.clone(),
                    (None, EmptyPolicy::Null, _) => Value::Null,
                    (None, EmptyPolicy::EmptyString, Kind::String) => Value::String(String::new()),
                    (None, ..) => continue,
                }
            } else {
                parse_value(text, col.kind, self.delimiter)
                    .map_err(|message| reject(format!("{}: {message}", col.header)))?
            };
            values.insert(col.attribute.clone(), value);
        }
        let key = match &plan.mapping.business_key {
            None => None,
            Some(attribute) => match values.get(attribute) {
                Some(Value::String(key)) if !key.is_empty() => Some(key.clone()),
                _ => return Err(reject("empty business key".into())),
            },
        };
        if values.is_empty() {
            return Err(reject("no values".into()));
        }
        Ok(MappedRow { row, key, values })
    }
}

/// A mapping checked against a file's headers.
pub struct Plan {
    mapping: Mapping,
    indices: Vec<usize>,
}
impl Plan {
    pub fn new(mapping: Mapping, headers: &[String]) -> Result<Self, Error> {
        if matches!(mapping.unknown_headers, UnknownHeaders::Reject) {
            let unknown: Vec<&str> = headers
                .iter()
                .filter(|h| !mapping.columns.iter().any(|c| &c.header == *h))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                return Err(Error::Header(format!(
                    "Unknown CSV columns: {}.",
                    name_list(&unknown)
                )));
            }
        }
        let indices = mapping
            .columns
            .iter()
            .map(|c| {
                headers
                    .iter()
                    .position(|h| h == &c.header)
                    .ok_or_else(|| Error::Header(format!("Missing CSV column: {}.", c.header)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { mapping, indices })
    }
}

/// The header's bytes without a byte order mark, and how much it consumed.
fn read_header<R: Read>(source: &mut BufReader<R>) -> Result<(Vec<u8>, u64), Error> {
    let mut raw = Vec::new();
    let mut quoted = false;
    let mut consumed = 0u64;
    let mut carriage_return = false;
    loop {
        let available = source.fill_buf()?;
        let Some(&byte) = available.first() else {
            break;
        };
        if carriage_return {
            if byte == b'\n' {
                source.consume(1);
                consumed += 1;
            }
            break;
        }
        source.consume(1);
        consumed += 1;
        match byte {
            b'"' => quoted = !quoted,
            b'\n' if !quoted => break,
            b'\r' if !quoted => {
                carriage_return = true;
                continue;
            }
            _ => {}
        }
        raw.push(byte);
        if raw.len() > MAX_RECORD_BYTES {
            return Err(Error::Header(
                "The header row exceeds 64 KiB, or a quote is never closed.".into(),
            ));
        }
    }
    if let Some(stripped) = raw.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        raw = stripped.to_vec();
    }
    Ok((raw, consumed))
}

fn detect_delimiter(header: &[u8]) -> u8 {
    let mut counts = [(b',', 0), (b';', 0), (b'\t', 0)];
    let mut quoted = false;
    for &byte in header {
        if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && let Some(count) = counts.iter_mut().find(|(d, _)| *d == byte) {
            count.1 += 1;
        }
    }
    // The first with the highest count, so a tie stays a comma.
    counts
        .iter()
        .fold(
            (b',', 0),
            |best, &next| if next.1 > best.1 { next } else { best },
        )
        .0
}

fn parse_header(raw: &[u8], delimiter: u8) -> Result<Vec<String>, Error> {
    if raw.is_empty() {
        return Err(Error::Header("The file has no header row.".into()));
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .delimiter(delimiter)
        .from_reader(raw);
    let mut record = csv::ByteRecord::new();
    reader.read_byte_record(&mut record)?;
    let mut seen = HashSet::new();
    let mut headers = Vec::with_capacity(record.len());
    for raw in record.iter() {
        let name = std::str::from_utf8(raw)
            .map_err(|_| Error::Header("The header row is not valid UTF-8.".into()))?;
        if name.trim().is_empty() {
            return Err(Error::Header("The header row has an empty column.".into()));
        }
        if !seen.insert(name.to_string()) {
            return Err(Error::Header(format!("Duplicate CSV column: {name}.")));
        }
        headers.push(name.to_string());
    }
    Ok(headers)
}

fn parse_value(text: &str, kind: Kind, delimiter: u8) -> Result<Value, &'static str> {
    match kind {
        Kind::String => Ok(Value::String(unescape_cell(text).into())),
        Kind::Date | Kind::Datetime | Kind::Time => Ok(Value::String(text.trim().into())),
        Kind::Integer => text
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "invalid integer"),
        Kind::Number => {
            let text = text.trim();
            // Spreadsheets in locales that separate fields with semicolons
            // write decimal commas.
            let decimal = (delimiter == b';').then(|| text.replacen(',', ".", 1));
            decimal
                .as_deref()
                .unwrap_or(text)
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or("invalid number")
        }
        Kind::Boolean => match text.trim() {
            t if t.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
            t if t.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
            _ => Err("expected true or false"),
        },
        Kind::Json => serde_json::from_str(text).map_err(|_| "invalid JSON"),
    }
}

fn is_formula(text: &str) -> bool {
    const SIGILS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];
    text.starts_with(SIGILS) || text.trim_start().starts_with(SIGILS)
}

/// Formulas are prefixed with an apostrophe, including those with leading whitespace.
/// Consumers must not strip this prefix before opening in a spreadsheet.
pub fn safe_cell(text: &str) -> String {
    if is_formula(text) {
        format!("'{text}")
    } else {
        text.into()
    }
}

/// Undoes `safe_cell`, so an exported file imports back unchanged.
fn unescape_cell(text: &str) -> &str {
    match text.strip_prefix('\'') {
        Some(rest) if is_formula(rest) => rest,
        _ => text,
    }
}

/// One value as a cell. Only text can hold a formula; a negative number stays a number.
pub fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => safe_cell(s),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::Array(_) | Value::Object(_) => safe_cell(&value.to_string()),
    }
}

/// A selection member's value as a cell. Selection pages wrap each value with
/// the context it resolved from.
pub fn selection_cell(value: &Value) -> String {
    cell(value.get("value").unwrap_or(value))
}

pub fn export_row(profile: &ExportProfile, values: &HashMap<String, Value>) -> Vec<String> {
    profile
        .columns
        .iter()
        .map(|c| cell(values.get(&c.attribute).unwrap_or(&Value::Null)))
        .collect()
}

/// The UTF-8 byte order mark that makes spreadsheets read a file as UTF-8.
pub const BOM: &[u8] = &[0xef, 0xbb, 0xbf];

/// LF-terminated CSV bytes for `rows`, led by a BOM and `header` when given.
pub fn encode_rows<H: AsRef<[u8]>>(
    header: Option<&[H]>,
    bom: bool,
    rows: &[Vec<String>],
) -> Result<Vec<u8>, Error> {
    let start = if bom { BOM.to_vec() } else { Vec::new() };
    let mut writer = csv::Writer::from_writer(start);
    if let Some(header) = header {
        writer.write_record(header)?;
    }
    for row in rows {
        writer.write_record(row)?;
    }
    writer
        .into_inner()
        .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))
}

/// A part of the rejection report: the row number and reason of each rejected
/// row, never its values. `first` adds the header.
pub fn report_rows(first: bool, rejections: &[(u64, String)]) -> Result<Vec<u8>, Error> {
    let rows: Vec<Vec<String>> = rejections
        .iter()
        .map(|(row, message)| vec![row.to_string(), safe_cell(bounded(message))])
        .collect();
    encode_rows(first.then_some(&["row", "error"][..]), false, &rows)
}

fn bounded(message: &str) -> &str {
    let mut end = message.len().min(MAX_REPORT_MESSAGE_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
            empty: EmptyPolicy::Skip,
        }
    }
    struct Rows<R: Read> {
        reader: CsvReader<R>,
        plan: Plan,
    }
    impl<R: Read> Rows<R> {
        fn open(source: R, profile: ImportProfile, position: Position) -> Result<Self, Error> {
            let reader = CsvReader::open(source, position)?;
            let plan = Plan::new(Mapping::from(&profile), reader.headers())?;
            Ok(Self { reader, plan })
        }
        fn next_row(&mut self) -> Result<Option<MappedRow>, Error> {
            match self.reader.next_record()? {
                None => Ok(None),
                Some(record) => self.reader.map(&self.plan, &record).map(Some),
            }
        }
    }
    fn rows(csv: &[u8], profile: ImportProfile) -> Rows<&[u8]> {
        Rows::open(csv, profile, Position::default()).unwrap()
    }
    fn key(row: Result<Option<MappedRow>, Error>) -> String {
        row.unwrap().unwrap().key.unwrap()
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
        let mut rows = Rows::open(
            Single(b"\xef\xbb\xbfid,count\r\n\"A\nB\",2\r\nC,\r\n"),
            profile(),
            Position::default(),
        )
        .unwrap();
        assert_eq!(key(rows.next_row()), "A\nB");
        assert_eq!(rows.next_row().unwrap().unwrap().values["count"], 0);
        assert!(rows.next_row().unwrap().is_none());
    }
    #[test]
    fn resuming_continues_at_the_next_record_for_every_line_ending() {
        for newline in ["\n", "\r\n", "\r"] {
            let csv = ["id,count", "a,1", "\"b\nb\",2", "c,3", "d,4", ""].join(newline);
            let mut all = Vec::new();
            let mut position = Position::default();
            // One record per open, as a run with one row per batch would read it.
            loop {
                let mut reader = rows(csv.as_bytes(), profile());
                reader = Rows {
                    reader: CsvReader::open(csv.as_bytes(), position).unwrap(),
                    plan: reader.plan,
                };
                match reader.next_row().unwrap() {
                    None => break,
                    Some(row) => all.push((row.row, row.key.unwrap())),
                }
                position = reader.reader.position();
            }
            let expected = [(2, "a"), (3, "b\nb"), (4, "c"), (5, "d")]
                .map(|(row, key)| (row, key.to_string()));
            assert_eq!(all, expected, "{newline:?}");
        }
    }
    #[test]
    fn a_position_past_the_end_is_reported() {
        let position = Position {
            offset: 1000,
            row: 9,
        };
        assert!(matches!(
            CsvReader::open(&b"id,count\na,1\n"[..], position),
            Err(Error::Header(_))
        ));
    }
    #[test]
    fn semicolon_and_tab_files_are_detected() {
        let mut p = profile();
        p.columns[1].kind = Kind::Number;
        p.columns[1].default = None;
        let mut semicolon = rows(b"id;count\n\"a;b\";12,5\n", p.clone());
        let row = semicolon.next_row().unwrap().unwrap();
        assert_eq!(row.key.unwrap(), "a;b");
        assert_eq!(row.values["count"], 12.5);
        let mut tab = rows(b"id\tcount\na\t3\n", p.clone());
        assert_eq!(tab.next_row().unwrap().unwrap().values["count"], 3.0);
        // A decimal comma is only a decimal comma in a semicolon file.
        let mut comma = rows(b"id,count\na,\"1,5\"\n", p);
        assert!(matches!(comma.next_row(), Err(Error::Row { row: 2, .. })));
    }
    #[test]
    fn a_row_of_the_wrong_width_is_rejected_and_the_next_is_read() {
        let mut r = rows(b"id,count\na,1,extra\nb\nc,3\n", profile());
        assert!(matches!(r.next_row(), Err(Error::Row { row: 2, .. })));
        assert!(matches!(r.next_row(), Err(Error::Row { row: 3, .. })));
        assert_eq!(key(r.next_row()), "c");
    }
    #[test]
    fn rows_of_empty_cells_are_passed_over_and_still_counted() {
        let mut r = rows(b"id,count\n,\n\nb,3\n", profile());
        let row = r.next_row().unwrap().unwrap();
        assert_eq!(row.row, 3);
        assert_eq!(row.key.unwrap(), "b");
    }
    #[test]
    fn rejects_bad_headers_and_row() {
        assert!(Rows::open(&b"id,id\nx,y"[..], profile(), Position::default()).is_err());
        assert!(Rows::open(&b""[..], profile(), Position::default()).is_err());
        let mut r = rows(b"id,count\na,nope", profile());
        assert!(matches!(r.next_row(), Err(Error::Row { row: 2, .. })));
        let mut r = rows(b"id,count\n,3\n", profile());
        assert!(matches!(r.next_row(), Err(Error::Row { row: 2, .. })));
    }
    #[test]
    fn quoted_record_bound_spans_newlines_and_escaped_quotes() {
        let csv = format!(
            "id,count\n\"{}\"\"\n{}\",1\n",
            "x".repeat(35000),
            "y".repeat(35000)
        );
        let mut r = rows(csv.as_bytes(), profile());
        assert!(r.next_row().is_err());
        let mut r = rows(b"id,count\na\"b,2\nc,3\n", profile());
        assert_eq!(key(r.next_row()), "a\"b");
        assert_eq!(key(r.next_row()), "c");
    }
    #[test]
    fn oversized_record_is_rejected_before_allocation() {
        let csv = format!("id,count\n{},1\n", "x".repeat(70000));
        let mut r = rows(csv.as_bytes(), profile());
        assert!(r.next_row().is_err());
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
        let mut r = rows(b"id,count\na,\nb,4\n", p.clone());
        assert!(!r.next_row().unwrap().unwrap().values.contains_key("count"));
        assert_eq!(r.next_row().unwrap().unwrap().values["count"], 4);
        assert!(r.next_row().unwrap().is_none());
        p.empty = EmptyPolicy::Null;
        let mut r = rows(b"id,count\na,\n", p.clone());
        assert_eq!(r.next_row().unwrap().unwrap().values["count"], Value::Null);
        p.empty = EmptyPolicy::EmptyString;
        let mut r = rows(b"id,count\na,\n", p);
        assert!(!r.next_row().unwrap().unwrap().values.contains_key("count"));
        let mut invalid = profile();
        invalid.columns[1].default = Some(Value::String("wrong".into()));
        assert!(invalid.validate().is_err());
        assert!(
            Rows::open(
                &b"id,count,unexpected\na,2,x"[..],
                profile(),
                Position::default()
            )
            .is_err()
        );
    }
    #[test]
    fn without_a_business_key_rows_have_no_key() {
        let mapping = Mapping {
            columns: profile().columns,
            business_key: None,
            unknown_headers: UnknownHeaders::Reject,
            empty: EmptyPolicy::Skip,
        };
        let mut reader = CsvReader::open(&b"id,count\n,5\n"[..], Position::default()).unwrap();
        let plan = Plan::new(mapping, reader.headers()).unwrap();
        let record = reader.next_record().unwrap().unwrap();
        let row = reader.map(&plan, &record).unwrap();
        assert!(row.key.is_none());
        assert_eq!(row.values["count"], 5);
    }
    #[test]
    fn typed_cells_parse() {
        let parse = |text, kind| parse_value(text, kind, b',');
        assert_eq!(parse("TRUE", Kind::Boolean), Ok(json!(true)));
        assert_eq!(parse(" false ", Kind::Boolean), Ok(json!(false)));
        assert!(parse("yes", Kind::Boolean).is_err());
        assert_eq!(parse(" 12 ", Kind::Integer), Ok(json!(12)));
        assert!(parse("NaN", Kind::Number).is_err());
        assert_eq!(parse("2026-11-02", Kind::Date), Ok(json!("2026-11-02")));
        assert_eq!(parse("{\"a\":1}", Kind::Json), Ok(json!({"a":1})));
        assert!(parse("{", Kind::Json).is_err());
    }
    #[test]
    fn headers_map_to_attribute_codes() {
        let schema: Vec<SchemaAttribute> = serde_json::from_value(json!([
            {"id":"sku","kind":"string"},{"id":"qty","kind":"integer"},
            {"id":"image","kind":"file"},{"id":"Sku2","kind":"string"}
        ]))
        .unwrap();
        let columns = columns_from_headers(&["SKU ".into(), "qty".into()], &schema).unwrap();
        assert_eq!(columns[0].attribute, "sku");
        assert_eq!(columns[0].header, "SKU ");
        assert_eq!(columns[1].kind, Kind::Integer);
        let error = columns_from_headers(&["sku".into(), "nope".into()], &schema).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
        let error = columns_from_headers(&["image".into()], &schema).unwrap_err();
        assert!(error.to_string().contains("image"), "{error}");
        // Two columns for one attribute cannot both be written.
        assert!(columns_from_headers(&["sku".into(), "SKU".into()], &schema).is_err());
    }
    #[test]
    fn formula_and_quotes() {
        let output =
            encode_rows::<&str>(None, false, &[vec![safe_cell(" =1+1"), "a,\"b\n".into()]])
                .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "' =1+1,\"a,\"\"b\n\"\n");
    }
    #[test]
    fn only_text_is_escaped_and_it_imports_back_unchanged() {
        assert_eq!(cell(&json!(-5)), "-5");
        assert_eq!(cell(&json!(-1.5)), "-1.5");
        assert_eq!(cell(&json!(true)), "true");
        assert_eq!(cell(&Value::Null), "");
        assert_eq!(cell(&json!([1, 2])), "[1,2]");
        for text in ["=SUM(A1)", "-5", "@x", " +1", "plain", "it's"] {
            assert_eq!(
                parse_value(&cell(&json!(text)), Kind::String, b','),
                Ok(json!(text))
            );
        }
    }
    #[test]
    fn selection_values_are_unwrapped() {
        let wrapped = json!({"source_context": {"code": "default"}, "value": -3.5});
        assert_eq!(selection_cell(&wrapped), "-3.5");
        assert_eq!(selection_cell(&json!("plain")), "plain");
    }
    #[test]
    fn exports_keep_lf_and_lead_with_header_and_bom() {
        let bytes = encode_rows(Some(&["ID"]), false, &[]).unwrap();
        assert_eq!(bytes, b"ID\n");
        let bytes = encode_rows(Some(&["ID"]), true, &[vec!["ż".into()]]).unwrap();
        assert_eq!(bytes, "\u{feff}ID\nż\n".as_bytes());
    }
    #[test]
    fn the_report_names_rows_and_bounds_reasons() {
        let bytes = report_rows(true, &[(2, "é".repeat(400)), (7, "=bad".into())]).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("row,error\n2,"));
        assert!(text.ends_with("7,'=bad\n"));
        assert!(text.len() < 600);
    }
}
