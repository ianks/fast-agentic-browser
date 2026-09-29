//! fab's output contract. Every command prints JSON Lines on stdout, one
//! event per line: `start` first, then `record`s (or `item`s), then exactly
//! one `end`, on success and on failure alike. Progress goes to stderr, and
//! only with `-v`. See docs/output.md.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The contract's version, in every `start`. Bumped when the meaning of an
/// existing field changes; new fields do not bump it.
pub const VERSION: u32 = 1;

/// Why a command did not succeed. A closed set: consumers may match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The program does not parse or cannot satisfy the request; nothing ran.
    InvalidProgram,
    /// The command's arguments were rejected; nothing ran.
    InvalidArgs,
    NavigationFailed,
    StepFailed,
    BudgetExhausted,
    SecretUnavailable,
    /// A record did not fit the declared schema.
    SchemaMismatch,
    /// An effect may have happened: resolve it, then resume explicitly.
    Paused,
    Cancelled,
    /// The runner stopped (session closed, process restarted); resume explicitly.
    Interrupted,
    Internal,
}

impl ErrorCode {
    /// 2: rejected before any browser effect. 3: stopped with an outcome to
    /// reconcile. 1: failed after starting.
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorCode::InvalidProgram | ErrorCode::InvalidArgs => 2,
            ErrorCode::Paused | ErrorCode::Interrupted => 3,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
    /// The program line, for failures inside a program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Browser effects may have happened before the failure.
    #[serde(default)]
    pub uncertain: bool,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

/// A rejected argument or request: nothing ran.
pub fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::InvalidArgs, message)
}

impl Failure {
    /// The typed failure inside an error chain, or `internal`.
    pub fn of(error: &anyhow::Error) -> Self {
        error.chain().find_map(|e| e.downcast_ref::<Failure>().cloned()).unwrap_or_else(|| Failure::new(ErrorCode::Internal, format!("{error:#}")))
    }
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), line: None, uncertain: matches!(code, ErrorCode::Paused | ErrorCode::Interrupted) }
    }
    pub fn at(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }
}

/// The last line of every stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct End {
    pub ts: String,
    /// Where the browser was left (null without a page).
    pub url: Option<String>,
    pub ok: bool,
    /// The program's `return` value, or the step's answer; null if none.
    pub value: Value,
    /// Records the task has committed in all (the next `seq`).
    pub records: u64,
    pub error: Option<Failure>,
}

impl End {
    pub fn ok(value: Value) -> Self {
        Self { ts: now(), url: None, ok: true, value, records: 0, error: None }
    }
    pub fn fail(failure: Failure) -> Self {
        Self { ts: now(), url: None, ok: false, value: Value::Null, records: 0, error: Some(failure) }
    }
    pub fn exit_code(&self) -> ExitCode {
        match &self.error {
            None if self.ok => ExitCode::SUCCESS,
            Some(f) => ExitCode::from(f.code.exit_code()),
            None => ExitCode::from(1),
        }
    }
}

/// A record as committed: everything but its `seq`, which is its position
/// in the task's outbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub ts: String,
    /// The page the record was read from (null for a computed record).
    pub url: Option<String>,
    /// Always an object, keyed by name, in emit (or schema) order.
    pub data: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    Start {
        v: u32,
        ts: String,
        cmd: String,
        /// The durable task, when the command runs as one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        /// The shape every record has, when known up front.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<Value>,
    },
    Record {
        seq: u64,
        #[serde(flatten)]
        record: Record,
    },
    /// One result of a listing command (tasks, sessions, doctor, …).
    Item { data: Value },
    End(End),
}

// One process prints one stream.
static STARTED: AtomicBool = AtomicBool::new(false);
static ENDED: AtomicBool = AtomicBool::new(false);
// The next `seq` after the records printed so far.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);

/// `fab --schema`: JSON Schemas of a structured request (`fab do '{…}'`)
/// and of the events on stdout.
pub fn schemas() -> Value {
    let scalar = serde_json::json!({"type": "object", "description": "A flat scalar field.", "properties": {
        "type": {"oneOf": [
            {"enum": ["string", "number", "integer", "boolean"]},
            {"type": "array", "items": {"enum": ["string", "number", "integer", "boolean", "null"]}, "minItems": 2, "maxItems": 2}]},
        "format": {"const": "uri"}, "enum": {"type": "array", "minItems": 1}, "description": {"type": "string"}, "title": {"type": "string"}},
        "required": ["type"], "additionalProperties": false});
    let failure = serde_json::json!({"type": ["object", "null"], "properties": {
        "code": {"enum": ["invalid_program", "invalid_args", "navigation_failed", "step_failed", "budget_exhausted", "secret_unavailable", "schema_mismatch", "paused", "cancelled", "interrupted", "internal"]},
        "message": {"type": "string"}, "line": {"type": "integer"}, "uncertain": {"type": "boolean"}}, "required": ["code", "message", "uncertain"]});
    serde_json::json!({
        "request": {"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "additionalProperties": false, "required": ["do"], "properties": {
            "do": {"type": "string", "minLength": 1, "description": "The goal, in words."},
            "url": {"type": "string", "description": "Open this first."},
            "records": {"type": "object", "description": "JSON Schema of every record: flat scalar properties only.", "additionalProperties": false, "required": ["properties"], "properties": {
                "type": {"const": "object"}, "properties": {"type": "object", "minProperties": 1, "additionalProperties": scalar},
                "required": {"type": "array", "items": {"type": "string"}}, "additionalProperties": {"const": false},
                "description": {"type": "string"}, "title": {"type": "string"}, "$schema": {"type": "string"}}},
            "returns": scalar}},
        "event": {"$schema": "https://json-schema.org/draft/2020-12/schema", "oneOf": [
            {"type": "object", "required": ["t", "v", "ts", "cmd"], "properties": {"t": {"const": "start"}, "v": {"const": VERSION}, "ts": {"type": "string"}, "cmd": {"type": "string"}, "task": {"type": "string"}, "schema": {"type": "object"}}},
            {"type": "object", "required": ["t", "seq", "ts", "url", "data"], "properties": {"t": {"const": "record"}, "seq": {"type": "integer", "minimum": 0}, "ts": {"type": "string"}, "url": {"type": ["string", "null"]}, "data": {"type": "object"}}},
            {"type": "object", "required": ["t", "data"], "properties": {"t": {"const": "item"}, "data": {}}},
            {"type": "object", "required": ["t", "ts", "url", "ok", "value", "records", "error"], "properties": {"t": {"const": "end"}, "ts": {"type": "string"}, "url": {"type": ["string", "null"]}, "ok": {"type": "boolean"}, "value": {}, "records": {"type": "integer", "minimum": 0}, "error": failure}}]}
    })
}

/// Prints one command's stream. Guarantees `start` first and one `end` last.
pub struct Writer {
    cmd: String,
}

impl Writer {
    pub fn new(cmd: impl Into<String>) -> Self {
        Self { cmd: cmd.into() }
    }
    pub fn start(&mut self, task: Option<String>, schema: Option<Value>) {
        if !STARTED.swap(true, Ordering::SeqCst) {
            line(&Event::Start { v: VERSION, ts: now(), cmd: self.cmd.clone(), task, schema });
        }
    }
    /// A `start` or `record` relayed from a session, printed as received.
    pub fn relay(&mut self, event: &Event) {
        match event {
            // Printed as the session made it (its time), under this command's name.
            Event::Start { v, ts, task, schema, .. } => {
                if !STARTED.swap(true, Ordering::SeqCst) {
                    line(&Event::Start { v: *v, ts: ts.clone(), cmd: self.cmd.clone(), task: task.clone(), schema: schema.clone() });
                }
            }
            Event::End(_) => {}
            other => {
                self.start(None, None);
                if let Event::Record { seq, .. } = other {
                    NEXT_SEQ.fetch_max(seq + 1, Ordering::SeqCst);
                }
                line(other);
            }
        }
    }
    pub fn item(&mut self, data: Value) {
        self.start(None, None);
        line(&Event::Item { data });
    }
    pub fn end(mut self, mut end: End) -> ExitCode {
        self.start(None, None);
        // A stream that stopped early still counts what it delivered.
        end.records = end.records.max(NEXT_SEQ.load(Ordering::SeqCst));
        let code = end.exit_code();
        if !ENDED.swap(true, Ordering::SeqCst) {
            line(&Event::End(end));
        }
        code
    }
}

/// Ends the stream with `failure` unless it has already ended: the path for
/// errors that escape a command.
pub fn abort(cmd: &str, failure: Failure) -> ExitCode {
    Writer::new(cmd).end(End::fail(failure))
}

fn line(event: &Event) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, event);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Now, as RFC 3339 UTC with milliseconds: "2026-09-26T11:40:02.123Z".
pub fn now() -> String {
    timestamp(SystemTime::now())
}

fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, m, day) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", s / 3600, s % 3600 / 60, s % 60, d.subsec_millis())
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn timestamps_are_rfc3339_utc_millis() {
        let at = |ms: u64| timestamp(UNIX_EPOCH + Duration::from_millis(ms));
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(1_790_422_802_123), "2026-09-26T11:40:02.123Z");
        assert_eq!(at(4_107_542_399_999), "2100-02-28T23:59:59.999Z");
    }

    #[test]
    fn events_have_one_flat_shape() {
        let mut data = Map::new();
        data.insert("price".into(), json!(42));
        let record = Event::Record { seq: 3, record: Record { ts: "t".into(), url: None, data } };
        assert_eq!(serde_json::to_value(&record).unwrap(), json!({"t": "record", "seq": 3, "ts": "t", "url": null, "data": {"price": 42}}));
        let end = Event::End(End { ts: "t".into(), url: Some("https://a.test/".into()), ok: false, value: json!(null), records: 4, error: Some(Failure::new(ErrorCode::Paused, "lost reply").at(2)) });
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            json!({"t": "end", "ts": "t", "url": "https://a.test/", "ok": false, "value": null, "records": 4,
                   "error": {"code": "paused", "message": "lost reply", "line": 2, "uncertain": true}})
        );
        for e in [record, end] {
            assert_eq!(serde_json::from_value::<Event>(serde_json::to_value(&e).unwrap()).unwrap(), e);
        }
    }

    #[test]
    fn printed_schema_lists_every_error_code() {
        let codes = schemas()["event"]["oneOf"][3]["properties"]["error"]["properties"]["code"]["enum"].clone();
        let all = [ErrorCode::InvalidProgram, ErrorCode::InvalidArgs, ErrorCode::NavigationFailed, ErrorCode::StepFailed, ErrorCode::BudgetExhausted, ErrorCode::SecretUnavailable, ErrorCode::SchemaMismatch, ErrorCode::Paused, ErrorCode::Cancelled, ErrorCode::Interrupted, ErrorCode::Internal];
        assert_eq!(codes, serde_json::to_value(all).unwrap());
    }

    #[test]
    fn exit_codes_follow_the_end() {
        assert_eq!(End::ok(json!(1)).exit_code(), ExitCode::SUCCESS);
        assert_eq!(End::fail(Failure::new(ErrorCode::InvalidProgram, "x")).exit_code(), ExitCode::from(2));
        assert_eq!(End::fail(Failure::new(ErrorCode::Interrupted, "x")).exit_code(), ExitCode::from(3));
        assert_eq!(End::fail(Failure::new(ErrorCode::StepFailed, "x")).exit_code(), ExitCode::from(1));
    }
}
