pub use super::audit_json::{AuditValue, JsString};
pub(crate) use super::audit_json::{normalize_json, number_value};
pub(crate) use super::date_parser::parse_timestamp;
use chrono::{SecondsFormat, Utc};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditSource {
    Transport,
    Mcp,
    T3,
    Git,
    Journal,
    System,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditFields {
    pub event_id: JsString,
    pub timestamp: JsString,
    pub process_id: f64,
    pub source: AuditSource,
    pub event: JsString,
    pub correlation_id: Option<JsString>,
    pub parent_correlation_id: Option<JsString>,
    pub parent_operation: Option<JsString>,
    pub operation: Option<JsString>,
    pub outcome: Option<JsString>,
    pub duration_ms: Option<f64>,
    pub details: Option<AuditValue>,
}

#[derive(Clone, Debug)]
pub struct AuditEvent {
    fields: AuditFields,
    value: AuditValue,
}

impl Deref for AuditEvent {
    type Target = AuditFields;

    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl TryFrom<AuditValue> for AuditEvent {
    type Error = AuditError;

    fn try_from(mut value: AuditValue) -> Result<Self, Self::Error> {
        let record = value.as_object().ok_or_else(invalid_event)?;
        let get = |name: &str| {
            record
                .iter()
                .find(|(key, _)| key.as_str() == Some(name))
                .map(|(_, value)| value)
        };
        if get("version").and_then(AuditValue::as_f64) != Some(1.0) {
            return Err(invalid_event());
        }
        let optional_string = |name| match get(name) {
            None => Ok(None),
            Some(AuditValue::String(value)) => Ok(Some(JsString::from(value.clone()))),
            Some(AuditValue::Utf16String(value)) => Ok(Some(JsString::from_utf16(value.clone()))),
            Some(_) => Err(invalid_event()),
        };
        let required_string = |name| match get(name) {
            Some(AuditValue::String(value)) => Ok(JsString::from(value.clone())),
            Some(AuditValue::Utf16String(value)) => Ok(JsString::from_utf16(value.clone())),
            _ => Err(invalid_event()),
        };
        let process_id = get("processId")
            .and_then(AuditValue::as_f64)
            .ok_or_else(invalid_event)?;
        let source = match get("source").and_then(AuditValue::as_str) {
            Some("transport") => AuditSource::Transport,
            Some("mcp") => AuditSource::Mcp,
            Some("t3") => AuditSource::T3,
            Some("git") => AuditSource::Git,
            Some("journal") => AuditSource::Journal,
            Some("system") => AuditSource::System,
            _ => return Err(invalid_event()),
        };
        let duration_ms = match get("durationMs") {
            None => None,
            Some(AuditValue::Number(number)) => number.as_f64(),
            Some(_) => return Err(invalid_event()),
        };
        let details = match get("details") {
            None => None,
            Some(value @ AuditValue::Object(_)) => Some(value.clone()),
            Some(_) => return Err(invalid_event()),
        };
        let fields = AuditFields {
            event_id: required_string("eventId")?,
            timestamp: required_string("timestamp")?,
            process_id,
            source,
            event: required_string("event")?,
            correlation_id: optional_string("correlationId")?,
            parent_correlation_id: optional_string("parentCorrelationId")?,
            parent_operation: optional_string("parentOperation")?,
            operation: optional_string("operation")?,
            outcome: optional_string("outcome")?,
            duration_ms,
            details,
        };
        value.normalize();
        Ok(Self { fields, value })
    }
}

impl Serialize for AuditEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AuditEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(AuditValue::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug)]
pub struct AuditEventInput {
    pub source: AuditSource,
    pub event: String,
    pub correlation_id: Option<String>,
    pub operation: Option<String>,
    pub outcome: Option<String>,
    pub duration_ms: Option<f64>,
    pub details: Option<Value>,
}

impl AuditEventInput {
    pub fn new(source: AuditSource, event: impl Into<String>) -> Self {
        Self {
            source,
            event: event.into(),
            correlation_id: None,
            operation: None,
            outcome: None,
            duration_ms: None,
            details: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditQueryInput {
    pub since: Option<String>,
    pub until: Option<String>,
    pub source: Option<AuditSource>,
    pub event: Option<String>,
    pub operation: Option<String>,
    pub outcome: Option<String>,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for AuditQueryInput {
    fn default() -> Self {
        Self {
            since: None,
            until: None,
            source: None,
            event: None,
            operation: None,
            outcome: None,
            cursor: None,
            limit: 50,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogPage {
    pub items: Vec<AuditEvent>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub total: u64,
    pub invalid_lines: u64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditScan {
    pub invalid_lines: u64,
    pub bytes: u64,
    pub valid_lines: u64,
}

#[derive(Debug)]
pub enum AuditError {
    Io(io::Error),
    InvalidInput(String),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::InvalidInput(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<io::Error> for AuditError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn invalid_event() -> AuditError {
    AuditError::InvalidInput("invalid audit event".into())
}

#[derive(Clone, Debug)]
pub struct AuditContext {
    pub correlation_id: String,
    pub operation: String,
}

tokio::task_local! {
    static AUDIT_CONTEXT: AuditContext;
}

pub async fn with_audit_context<F: Future>(
    correlation_id: impl Into<String>,
    operation: impl Into<String>,
    action: F,
) -> F::Output {
    AUDIT_CONTEXT
        .scope(
            AuditContext {
                correlation_id: correlation_id.into(),
                operation: operation.into(),
            },
            action,
        )
        .await
}

pub(crate) fn current_audit_context() -> Option<AuditContext> {
    AUDIT_CONTEXT.try_with(Clone::clone).ok()
}

#[derive(Default)]
struct AuditState {
    initialized: bool,
    last_error: Option<String>,
}

pub struct AuditLog {
    pub file_path: PathBuf,
    state: Mutex<AuditState>,
}

impl AuditLog {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
            state: Mutex::new(AuditState::default()),
        }
    }

    pub fn diagnostic_error(&self) -> Option<String> {
        self.state.lock().unwrap().last_error.clone()
    }

    pub fn record(&self, input: AuditEventInput) {
        let mut state = self.state.lock().unwrap();
        let result = (|| -> Result<(), AuditError> {
            let mut event = Map::new();
            event.insert("version".into(), json!(1));
            event.insert("eventId".into(), json!(format!("evt_{}", Uuid::new_v4())));
            event.insert(
                "timestamp".into(),
                json!(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
            );
            event.insert("processId".into(), json!(std::process::id()));
            event.insert("source".into(), serde_json::to_value(input.source).unwrap());
            event.insert("event".into(), json!(input.event));
            if let Some(id) = &input.correlation_id {
                event.insert("correlationId".into(), json!(id));
            }
            let parent = AUDIT_CONTEXT.try_with(Clone::clone).ok();
            if let Some(parent) =
                parent.filter(|p| Some(&p.correlation_id) != input.correlation_id.as_ref())
            {
                event.insert("parentCorrelationId".into(), json!(parent.correlation_id));
                event.insert("parentOperation".into(), json!(parent.operation));
            }
            if let Some(operation) = input.operation {
                event.insert("operation".into(), json!(operation));
            }
            if let Some(outcome) = input.outcome {
                event.insert("outcome".into(), json!(outcome));
            }
            if let Some(duration) = input.duration_ms {
                event.insert(
                    "durationMs".into(),
                    number_value(if duration.is_nan() {
                        duration
                    } else {
                        (if duration - duration.floor() < 0.5 {
                            duration.floor()
                        } else {
                            duration.floor() + 1.0
                        })
                        .max(0.0)
                    }),
                );
            }
            let mut event = AuditValue::from(Value::Object(event));
            if let Some(details) = input.details {
                let summary = summarize_for_audit(&details);
                event.as_object_mut().unwrap().insert(
                    "details".into(),
                    if summary.is_object() {
                        summary
                    } else {
                        AuditValue::Object(indexmap::IndexMap::from_iter([(
                            "value".into(),
                            summary,
                        )]))
                    },
                );
            }
            let mut line = serde_json::to_vec(&event).unwrap();
            line.push(b'\n');
            self.init(&mut state)?;
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&self.file_path)?.write_all(&line)?;
            Ok(())
        })();
        if let Err(error) = result {
            state.last_error = Some(sanitize_error(&error.to_string()));
        }
    }

    pub fn query(&self, input: &AuditQueryInput) -> Result<AuditLogPage, AuditError> {
        let mut state = self.state.lock().unwrap();
        if let Err(error) = self.init(&mut state) {
            state.last_error = Some(sanitize_error(&error.to_string()));
            return Err(error.into());
        }
        let since = parse_filter_time(input.since.as_deref(), "since")?;
        let until = parse_filter_time(input.until.as_deref(), "until")?;
        let offset = parse_cursor(input.cursor.as_deref())?;
        let mut items = Vec::new();
        let mut total = 0;
        let scan = scan_audit_file(&self.file_path, |event| {
            if !in_time_window(&event.timestamp, since, until)
                || input.source.is_some_and(|source| event.source != source)
                || input
                    .event
                    .as_ref()
                    .is_some_and(|name| event.event.as_str() != Some(name.as_str()))
                || input.operation.as_ref().is_some_and(|name| {
                    event.operation.as_ref().and_then(JsString::as_str) != Some(name.as_str())
                })
                || input.outcome.as_ref().is_some_and(|name| {
                    event.outcome.as_ref().and_then(JsString::as_str) != Some(name.as_str())
                })
            {
                return;
            }
            if total >= offset && items.len() < input.limit {
                items.push(event);
            }
            total += 1;
        });
        let invalid_lines = match scan {
            Ok(scan) => scan.invalid_lines,
            Err(error) => {
                state.last_error = Some(sanitize_error(&error.to_string()));
                return Ok(AuditLogPage {
                    items: vec![],
                    next_cursor: None,
                    has_more: false,
                    total: 0,
                    invalid_lines: 0,
                });
            }
        };
        let next = offset + items.len() as u64;
        Ok(AuditLogPage {
            items,
            next_cursor: (next < total).then(|| next.to_string()),
            has_more: next < total,
            total,
            invalid_lines,
        })
    }

    fn init(&self, state: &mut AuditState) -> io::Result<()> {
        if state.initialized {
            return Ok(());
        }
        let parent = self
            .file_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match fs::set_permissions(&self.file_path, fs::Permissions::from_mode(0o600)) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        state.initialized = true;
        Ok(())
    }
}

pub fn scan_audit_file(
    path: impl AsRef<Path>,
    mut visit: impl FnMut(AuditEvent),
) -> io::Result<AuditScan> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(AuditScan::default()),
        Err(error) => return Err(error),
    };
    let bytes = file.metadata()?.len();
    let mut scan = AuditScan {
        bytes,
        ..AuditScan::default()
    };
    let mut reader = BufReader::new(file.take(bytes));
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let length = buffer.len();
        for &byte in buffer {
            if byte == b'\r' || byte == b'\n' {
                scan_line(&line, &mut scan, &mut visit);
                line.clear();
            } else {
                line.push(byte);
            }
        }
        reader.consume(length);
    }
    scan_line(&line, &mut scan, &mut visit);
    Ok(scan)
}

fn scan_line(line: &[u8], scan: &mut AuditScan, visit: &mut impl FnMut(AuditEvent)) {
    let line = String::from_utf8_lossy(line);
    if js_trim(&line).is_empty() {
        return;
    }
    match serde_json::from_str::<AuditEvent>(&line) {
        Ok(event) => {
            scan.valid_lines += 1;
            visit(event);
        }
        Err(_) => scan.invalid_lines += 1,
    }
}

pub fn hash_audit_content(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn summarize_for_audit(value: &Value) -> AuditValue {
    summarize_value(value, None, 0)
}

fn summarize_value(value: &Value, key: Option<&str>, depth: usize) -> AuditValue {
    let sensitive = key.is_some_and(is_sensitive_key);
    let normalized = key.map(normalize_key);
    if sensitive
        && (matches!(
            normalized.as_deref(),
            Some("answers" | "answer" | "options" | "payload" | "content")
        ) || !(value.is_object() || value.is_array()))
    {
        return redacted_value(value).into();
    }
    match value {
        Value::Null | Value::Bool(_) => value.clone().into(),
        Value::Number(number) => number_value(number.as_f64().unwrap_or(f64::NAN)).into(),
        Value::String(value) => {
            if sensitive {
                return redacted_string(value).into();
            }
            let sanitized = sanitize_error(value);
            if sanitized.encode_utf16().count() > 512 {
                let mut result = AuditValue::from(
                    json!({"type":"string", "length":value.encode_utf16().count(), "sha256":hash_audit_content(value)}),
                );
                let fields = result.as_object_mut().unwrap();
                fields.insert(
                    "preview".into(),
                    AuditValue::from_utf16(sanitized.encode_utf16().take(512).collect()),
                );
                fields.insert("truncated".into(), AuditValue::Bool(true));
                result
            } else {
                json!(sanitized).into()
            }
        }
        Value::Array(_) | Value::Object(_) if depth >= 6 => {
            json!({"type": if value.is_array() { "array" } else { "object" }, "truncated":true})
                .into()
        }
        Value::Array(values) => {
            let items: Vec<_> = values
                .iter()
                .take(25)
                .map(|v| summarize_value(v, key, depth + 1))
                .collect();
            if values.len() <= 25 {
                AuditValue::Array(items)
            } else {
                AuditValue::Object(indexmap::IndexMap::from_iter([
                    ("items".into(), AuditValue::Array(items)),
                    ("length".into(), json!(values.len()).into()),
                    ("truncated".into(), AuditValue::Bool(true)),
                ]))
            }
        }
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let mut output = indexmap::IndexMap::new();
            for key in keys.iter().take(50) {
                if key.as_str() == "__proto__" {
                    continue;
                }
                output.insert(
                    (*key).clone().into(),
                    summarize_value(&values[*key], Some(key), depth + 1),
                );
            }
            if keys.len() > 50 {
                output.insert("_truncatedKeys".into(), json!(keys.len() - 50).into());
            }
            let mut output = AuditValue::Object(output);
            output.normalize();
            output
        }
    }
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn is_sensitive_key(key: &str) -> bool {
    let key = normalize_key(key);
    [
        "authorization",
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "token",
        "secret",
        "password",
        "credential",
        "cookie",
        "apikey",
        "privatekey",
        "idempotencykey",
        "base64",
        "relativeurl",
        "uploadurl",
        "webhookurl",
    ]
    .iter()
    .any(|term| key.contains(term))
        || [
            "message",
            "instruction",
            "prompt",
            "text",
            "patch",
            "content",
            "answers",
            "answer",
            "body",
            "payload",
            "options",
        ]
        .contains(&key.as_str())
}

fn redacted_string(value: &str) -> Value {
    json!({"redacted":true, "type":"string", "length":value.encode_utf16().count(), "sha256":hash_audit_content(value)})
}

fn redacted_value(value: &Value) -> Value {
    match value {
        Value::String(value) => redacted_string(value),
        Value::Array(values) => json!({"redacted":true, "type":"array", "length":values.len()}),
        Value::Object(values) => json!({"redacted":true, "type":"object", "keyCount":values.len()}),
        _ => {
            json!({"redacted":true, "type":match value { Value::Bool(_) => "boolean", Value::Number(_) => "number", _ => "object" }})
        }
    }
}

const JS_SPACE: &str = r"[\t\n\v\f\r \x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";
static BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i-u:Bearer){JS_SPACE}+[^{space},;]+",
        space = &JS_SPACE[1..JS_SPACE.len() - 1]
    ))
    .unwrap()
});
static SECRET_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)sk-[A-Za-z0-9_-]{12,}(?-u:\b)").unwrap());
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?-u:\b)((?i-u:api[_-]?key|access[_-]?token|authorization|password|secret)){JS_SPACE}*[:=]{JS_SPACE}*[^{space},;]+", space = &JS_SPACE[1..JS_SPACE.len()-1])).unwrap()
});

pub fn sanitize_error(value: &str) -> String {
    let bearer = BEARER.replace_all(value, "Bearer [REDACTED]");
    let keys = SECRET_KEY.replace_all(&bearer, "[REDACTED]");
    SECRET_ASSIGNMENT
        .replace_all(&keys, "${1}=[REDACTED]")
        .into_owned()
}

pub(crate) fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n'
                | '\u{000b}'
                | '\u{000c}'
                | '\r'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

fn parse_cursor(cursor: Option<&str>) -> Result<u64, AuditError> {
    let cursor = js_trim(cursor.unwrap_or(""));
    if cursor.is_empty() {
        return Ok(0);
    }
    let radix = [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ];
    let number = radix
        .iter()
        .find_map(|(prefix, radix)| {
            cursor.strip_prefix(prefix).map(|s| {
                if s.is_empty() || !s.chars().all(|c| c.is_digit(*radix)) {
                    return None;
                }
                u64::from_str_radix(s, *radix).ok().map(|v| v as f64)
            })
        })
        .unwrap_or_else(|| cursor.parse::<f64>().ok());
    match number {
        Some(value)
            if value.is_finite()
                && value >= 0.0
                && value.fract() == 0.0
                && value <= 9_007_199_254_740_991.0 =>
        {
            Ok(value as u64)
        }
        _ => Err(AuditError::InvalidInput(
            "cursor must be a non-negative integer string.".into(),
        )),
    }
}

pub fn parse_filter_time(value: Option<&str>, name: &str) -> Result<Option<i64>, AuditError> {
    let Some(value) = value.filter(|v| !js_trim(v).is_empty()) else {
        return Ok(None);
    };
    parse_timestamp(value)
        .map(Some)
        .ok_or_else(|| AuditError::InvalidInput(format!("{name} must be a valid ISO date.")))
}

pub(crate) fn in_time_window(timestamp: &JsString, since: Option<i64>, until: Option<i64>) -> bool {
    if since.is_none() && until.is_none() {
        return true;
    }
    timestamp
        .as_str()
        .and_then(parse_timestamp)
        .is_some_and(|time| {
            since.is_none_or(|since| time >= since) && until.is_none_or(|until| time <= until)
        })
}

#[cfg(test)]
#[path = "audit_log_tests.rs"]
mod tests;
