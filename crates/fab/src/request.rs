//! Schema-first requests: an agent sends the goal and the exact shape of what
//! it wants back, so fab never guesses what to emit.
//!
//! ```json
//! {"do": "every job on all pages: title and salary", "url": "jobs.example.com",
//!  "records": {"type": "object",
//!              "properties": {"title": {"type": "string"}, "salary": {"type": ["number", "null"]}},
//!              "required": ["title"]}}
//! ```
//!
//! `records` and `returns` are standard JSON Schema, restricted to flat
//! scalars: any JSON Schema tool reads them, and a small checker here
//! validates them. Anything outside the subset is rejected by keyword and
//! JSON pointer.

use crate::events::{ErrorCode, Failure};
use fab_core::script::Value as ScriptValue;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// `fab do '{"do": …}'`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoRequest {
    #[serde(rename = "do")]
    pub goal: String,
    #[serde(default)]
    pub url: Option<String>,
    /// JSON Schema of every record.
    #[serde(default)]
    pub records: Option<Value>,
    /// JSON Schema of `end.value`.
    #[serde(default)]
    pub returns: Option<Value>,
}

impl DoRequest {
    /// A body that starts with `{` is a request; it never falls back to words.
    pub fn parse(body: &str) -> Result<Option<Self>, Failure> {
        if !body.trim_start().starts_with('{') {
            return Ok(None);
        }
        let req: DoRequest = serde_json::from_str(body).map_err(|e| invalid(format!("request: {e}")))?;
        if req.goal.trim().is_empty() {
            return Err(invalid("request: \"do\" must say what to do"));
        }
        if let Some(s) = &req.records {
            RecordSchema::parse(s)?;
        }
        if let Some(s) = &req.returns {
            Field::parse(s, "/returns", "returns", true)?;
        }
        Ok(Some(req))
    }
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::InvalidArgs, message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    String,
    Number,
    Integer,
    Boolean,
}

/// One scalar property.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub nullable: bool,
    pub required: bool,
    /// `format: "uri"`: relative references resolve against the record's page.
    pub uri: bool,
    pub allowed: Option<Vec<Value>>,
    pub description: Option<String>,
}

const FIELD_KEYWORDS: &[&str] = &["type", "format", "enum", "description", "title"];

impl Field {
    fn parse(schema: &Value, at: &str, name: &str, required: bool) -> Result<Self, Failure> {
        let obj = schema.as_object().ok_or_else(|| invalid(format!("{at}: a property schema is an object")))?;
        if let Some(k) = obj.keys().find(|k| !FIELD_KEYWORDS.contains(&k.as_str())) {
            return Err(invalid(format!("{at}/{k}: unsupported keyword (fields are flat scalars: type, format, enum, description)")));
        }
        let kind_of = |t: &str| match t {
            "string" => Some(Kind::String),
            "number" => Some(Kind::Number),
            "integer" => Some(Kind::Integer),
            "boolean" => Some(Kind::Boolean),
            _ => None,
        };
        let bad_type = || invalid(format!("{at}/type: one of string, number, integer, boolean, or [that, \"null\"]"));
        let (kind, nullable) = match obj.get("type") {
            Some(Value::String(t)) => (kind_of(t).ok_or_else(bad_type)?, false),
            Some(Value::Array(ts)) => match ts.iter().map(Value::as_str).collect::<Option<Vec<_>>>().as_deref() {
                Some([t, "null"]) | Some(["null", t]) => (kind_of(t).ok_or_else(bad_type)?, true),
                _ => return Err(bad_type()),
            },
            _ => return Err(bad_type()),
        };
        let uri = match obj.get("format") {
            None => false,
            Some(f) if f == "uri" && kind == Kind::String => true,
            Some(_) => return Err(invalid(format!("{at}/format: only \"uri\", on a string"))),
        };
        let fits = |v: &Value| match kind {
            Kind::String => v.is_string(),
            Kind::Number => v.is_number(),
            Kind::Integer => v.as_f64().is_some_and(|n| n.fract() == 0.0),
            Kind::Boolean => v.is_boolean(),
        };
        let allowed = match obj.get("enum") {
            None => None,
            Some(Value::Array(vs)) if !vs.is_empty() && vs.iter().all(fits) => Some(vs.clone()),
            Some(_) => return Err(invalid(format!("{at}/enum: a nonempty list of values of the field's type"))),
        };
        for k in ["description", "title"] {
            if obj.get(k).is_some_and(|v| !v.is_string()) {
                return Err(invalid(format!("{at}/{k}: a string")));
            }
        }
        let description = obj.get("description").and_then(Value::as_str).map(str::to_string);
        Ok(Self { name: name.to_string(), kind, nullable, required, uri, allowed, description })
    }

    /// The field as the compiler is told it: "votes (a whole number, may be
    /// missing): points shown under the title".
    fn describe(&self) -> String {
        match &self.description {
            Some(d) => format!("{} ({}): {d}", self.name, self.kind_text()),
            None => format!("{} ({})", self.name, self.kind_text()),
        }
    }

    /// The type in words: "a number, may be missing".
    pub fn kind_text(&self) -> String {
        let mut s = match self.kind {
            Kind::String if self.uri => "a URL",
            Kind::String => "text",
            Kind::Number => "a number",
            Kind::Integer => "a whole number",
            Kind::Boolean => "true or false",
        }
        .to_string();
        if let Some(a) = &self.allowed {
            s.push_str(&format!(", one of {}", a.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")));
        }
        if !self.required || self.nullable {
            s.push_str(", may be missing");
        }
        s
    }

    /// `value` as this field's type, with the script's coercions: "$1,299"
    /// is 1299, "yes" is true, a relative link resolves against `page`.
    pub fn coerce(&self, value: Option<&Value>, page: Option<&str>) -> Result<Value, String> {
        let value = match value {
            None | Some(Value::Null) if self.nullable || !self.required => return Ok(Value::Null),
            None | Some(Value::Null) => return Err("missing".into()),
            Some(v) => v,
        };
        let scalar = ScriptValue::from_json(value);
        // Blank text is a missing value, not an empty string or the page URL.
        if value.as_str().is_some_and(|s| s.trim().is_empty()) && (self.uri || self.kind != Kind::String) {
            return self.coerce(None, page);
        }
        let out = match self.kind {
            Kind::String => match value {
                Value::String(s) if self.uri => json!(resolve(s.trim(), page).ok_or_else(|| format!("{value} is not a link"))?),
                Value::String(_) => value.clone(),
                Value::Number(_) | Value::Bool(_) if !self.uri => json!(scalar.to_string()),
                // Rich text read as lines: text, one line each.
                Value::Array(lines) if !self.uri && !lines.is_empty() && lines.iter().all(Value::is_string) => {
                    json!(lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
                }
                _ => return Err(format!("{value} is not text")),
            },
            Kind::Number => number_json(strict_number(value).ok_or_else(|| format!("{value} is not a number"))?),
            Kind::Integer => {
                let n = strict_number(value).filter(|n| n.fract() == 0.0 && n.abs() < 9.0e15).ok_or_else(|| format!("{value} is not a whole number"))?;
                json!(n as i64)
            }
            Kind::Boolean => match value {
                Value::Bool(_) => value.clone(),
                Value::String(s) => match s.trim().to_lowercase().as_str() {
                    "true" | "yes" => json!(true),
                    "false" | "no" => json!(false),
                    _ => return Err(format!("{value} is not true or false")),
                },
                _ => return Err(format!("{value} is not true or false")),
            },
        };
        match &self.allowed {
            None => Ok(out),
            Some(allowed) => allowed
                .iter()
                .find(|a| **a == out || matches!((a.as_f64(), out.as_f64()), (Some(a), Some(o)) if a == o) || matches!((a.as_str(), out.as_str()), (Some(a), Some(o)) if a.eq_ignore_ascii_case(o.trim())))
                .cloned()
                .ok_or_else(|| format!("{out} is not one of the allowed values")),
        }
    }
}

/// A link, absolute when `page` is known; None for text that isn't one
/// ("N/A", "see below").
fn resolve(link: &str, page: Option<&str>) -> Option<String> {
    let shaped = !link.is_empty()
        && !link.chars().any(char::is_whitespace)
        && (reqwest::Url::parse(link).is_ok() || ["/", "./", "../", "?", "#"].iter().any(|p| link.starts_with(p)) || link.contains('.'));
    if !shaped {
        return None;
    }
    Some(page.and_then(|p| reqwest::Url::parse(p).ok()).and_then(|base| base.join(link).ok()).map(String::from).unwrap_or_else(|| link.to_string()))
}

/// A whole number as an integer, else a float.
fn number_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 { json!(n as i64) } else { json!(n) }
}

/// The one number a value states: a JSON number, or text with exactly one
/// number decorated by a currency, grouping or a unit ("$1,299.50",
/// "721 points", "169 m²"). Text with other digits ("call 555-1234",
/// "2024-05-01", "10-20"), a magnitude suffix ("1.2k") or a boolean is not
/// a number.
fn strict_number(value: &Value) -> Option<f64> {
    let s = match value {
        Value::Number(n) => return n.as_f64(),
        Value::String(s) => s.trim(),
        _ => return None,
    };
    let cs: Vec<char> = s.chars().collect();
    let start = cs.iter().position(char::is_ascii_digit)?;
    let mut end = start;
    let mut text = String::new();
    while end < cs.len() {
        let c = cs[end];
        let next_digit = cs.get(end + 1).is_some_and(char::is_ascii_digit);
        if c.is_ascii_digit() {
            text.push(c);
        } else if c == '.' && next_digit && !text.contains('.') {
            text.push(c);
        } else if c == ',' && next_digit && !text.contains('.') && cs[end + 1..].iter().take_while(|c| c.is_ascii_digit()).count() == 3 {
        } else {
            break;
        }
        end += 1;
    }
    let (before, after) = (&cs[..start], &cs[end..]);
    if after.iter().any(char::is_ascii_digit) {
        return None;
    }
    // "1.2k", "3M": a magnitude, not the number shown.
    if after.first().is_some_and(|c| c.is_alphabetic()) && after.iter().take_while(|c| c.is_alphabetic()).count() == 1 && matches!(after[0], 'k' | 'K' | 'm' | 'M' | 'b' | 'B') {
        return None;
    }
    // A minus sign only when nothing but decoration precedes it ("-5", "$-5"),
    // not a hyphen inside a word ("SKU-5").
    let negative = before.last().is_some_and(|c| *c == '-' || *c == '\u{2212}') && !before[..before.len() - 1].iter().any(|c| c.is_alphanumeric());
    if before.iter().any(|c| c.is_alphanumeric()) && !negative && before.last().is_some_and(|c| *c == '-') {
        return None;
    }
    let n: f64 = text.parse().ok()?;
    Some(if negative { -n } else { n })
}

/// The shape of every record: flat, ordered scalar fields.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSchema {
    pub fields: Vec<Field>,
    /// As the caller wrote it, for `start.schema`.
    pub raw: Value,
}

impl RecordSchema {
    pub fn parse(schema: &Value) -> Result<Self, Failure> {
        let obj = schema.as_object().ok_or_else(|| invalid("/records: a JSON Schema object"))?;
        for (k, v) in obj {
            match k.as_str() {
                "type" if v == "object" => {}
                "type" => return Err(invalid("/records/type: records are objects (\"object\")")),
                "properties" | "required" | "description" | "title" | "$schema" => {}
                "additionalProperties" if v == false => {}
                other => return Err(invalid(format!("/records/{other}: unsupported keyword (records are flat: type, properties, required)"))),
            }
        }
        let props = obj.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty()).ok_or_else(|| invalid("/records/properties: name at least one field"))?;
        let required: Vec<&str> = match obj.get("required") {
            None => vec![],
            Some(Value::Array(r)) => r.iter().map(|v| v.as_str().ok_or_else(|| invalid("/records/required: a list of property names"))).collect::<Result<_, _>>()?,
            Some(_) => return Err(invalid("/records/required: a list of property names")),
        };
        if let Some(r) = required.iter().find(|r| !props.contains_key(**r)) {
            return Err(invalid(format!("/records/required: \"{r}\" is not a property")));
        }
        let fields = props
            .iter()
            .map(|(name, s)| Field::parse(s, &format!("/records/properties/{}", pointer_escape(name)), name, required.contains(&name.as_str())))
            .collect::<Result<_, _>>()?;
        // `start.schema` states what records hold: a field that isn't required
        // is emitted as null when missing, so it is nullable there.
        let mut raw = schema.clone();
        for (name, s) in raw["properties"].as_object_mut().into_iter().flatten() {
            if !required.contains(&name.as_str()) {
                if let Some(t) = s["type"].as_str().map(str::to_string) {
                    s["type"] = json!([t, "null"]);
                }
            }
        }
        Ok(Self { fields, raw })
    }

    /// A record in schema order, every field present (null when missing),
    /// or the field that does not fit.
    pub fn conform(&self, data: &Map<String, Value>, page: Option<&str>) -> Result<Map<String, Value>, (String, Value)> {
        self.fields
            .iter()
            .map(|f| {
                let got = data.get(&f.name);
                f.coerce(got, page).map(|v| (f.name.clone(), v)).map_err(|_| (f.name.clone(), got.cloned().unwrap_or(Value::Null)))
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.name.clone()).collect()
    }

    /// What the compiler is told about the records.
    pub fn instruction(&self) -> String {
        format!(
            "Each `emit` must write exactly these fields, named exactly so, in this order: {}. Use `emit name: value, …` with these names; no other fields.",
            self.fields.iter().map(Field::describe).collect::<Vec<_>>().join("; ")
        )
    }
}

/// The schema of `end.value`.
pub fn returns(schema: &Value) -> Result<Field, Failure> {
    Field::parse(schema, "/returns", "returns", true)
}

fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// The shape a compiled program's records will have, when every `emit` names
/// its fields: properties with no type (`{}`, any value).
pub fn inferred(program: &fab_core::script::Program) -> Option<Value> {
    use fab_core::script::{Expr, Op};
    let mut props = Map::new();
    for op in program.ops() {
        let Op::Emit(es) = op else { continue };
        for (key, e) in es {
            let k = match (key, e) {
                (Some(k), _) => k.clone(),
                (None, Expr::Var(v)) if v.contains('.') => v.rsplit('.').next().unwrap_or(v).to_string(),
                // A bare variable may spread a record: its fields aren't known.
                _ => return None,
            };
            props.entry(k).or_insert(json!({}));
        }
    }
    (!props.is_empty()).then(|| json!({"type": "object", "properties": props}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(v: Value) -> Result<RecordSchema, String> {
        RecordSchema::parse(&v).map_err(|f| f.message)
    }

    #[test]
    fn requests_are_json_objects_or_words() {
        assert!(DoRequest::parse("open example.com").unwrap().is_none());
        let r = DoRequest::parse(r#" {"do": "list jobs", "url": "jobs.test"}"#).unwrap().unwrap();
        assert_eq!((r.goal.as_str(), r.url.as_deref()), ("list jobs", Some("jobs.test")));
        for bad in [r#"{"do": "x", "extra": 1}"#, r#"{"url": "x"}"#, "{not json", r#"{"do": " "}"#] {
            assert_eq!(DoRequest::parse(bad).unwrap_err().code, ErrorCode::InvalidArgs, "{bad}");
        }
    }

    #[test]
    fn only_flat_scalar_schemas_are_accepted() {
        let ok = schema(json!({"type": "object", "additionalProperties": false,
            "properties": {"b": {"type": "string", "format": "uri"}, "a": {"type": ["integer", "null"], "description": "votes"}, "c": {"type": "string", "enum": ["Open", "Closed"]}},
            "required": ["b"]}))
        .unwrap();
        assert_eq!(ok.names(), ["b", "a", "c"], "properties keep their order");
        assert!(ok.fields[0].required && ok.fields[0].uri && ok.fields[1].nullable && !ok.fields[1].required);
        for (bad, pointer) in [
            (json!({"type": "array"}), "/records/type"),
            (json!({"properties": {}}), "/records/properties"),
            (json!({"properties": {"x": {"type": "object"}}}), "/records/properties/x/type"),
            (json!({"properties": {"x": {"type": "array", "items": {}}}}), "/records/properties/x/items"),
            (json!({"properties": {"a/b": {"$ref": "#/x"}}}), "/records/properties/a~1b/$ref"),
            (json!({"properties": {"x": {"type": "string", "pattern": "^a"}}}), "/records/properties/x/pattern"),
            (json!({"properties": {"x": {"type": ["string", "number"]}}}), "/records/properties/x/type"),
            (json!({"properties": {"x": {"type": "number", "format": "uri"}}}), "/records/properties/x/format"),
            (json!({"oneOf": []}), "/records/oneOf"),
            (json!({"properties": {"x": {"type": "string"}}, "required": ["y"]}), "/records/required"),
        ] {
            let e = schema(bad.clone()).unwrap_err();
            assert!(e.starts_with(pointer), "{bad} → {e}");
        }
    }

    #[test]
    fn records_conform_in_schema_order_with_coercion() {
        let s = schema(json!({"properties": {
            "title": {"type": "string"}, "price": {"type": "number"}, "votes": {"type": "integer"},
            "link": {"type": "string", "format": "uri"}, "open": {"type": "boolean"},
            "state": {"type": "string", "enum": ["Open", "Closed"]}, "note": {"type": ["string", "null"]}},
            "required": ["title", "price"]}))
        .unwrap();
        let data = json!({"link": "/p/1", "price": "$1,299.50", "title": 7, "votes": "42 points", "open": "yes", "state": "closed", "extra": 1});
        let got = s.conform(data.as_object().unwrap(), Some("https://shop.test/list?page=2")).unwrap();
        assert_eq!(
            Value::Object(got.clone()),
            json!({"title": "7", "price": 1299.5, "votes": 42, "link": "https://shop.test/p/1", "open": true, "state": "Closed", "note": null})
        );
        assert_eq!(got.keys().cloned().collect::<Vec<_>>(), s.names());
        assert_eq!(s.conform(json!({"title": "x"}).as_object().unwrap(), None).unwrap_err().0, "price");
        assert_eq!(s.conform(json!({"title": "x", "price": "n/a"}).as_object().unwrap(), None).unwrap_err(), ("price".into(), json!("n/a")));
        assert_eq!(s.conform(json!({"title": "x", "price": 1, "votes": 1.5}).as_object().unwrap(), None).unwrap_err().0, "votes");
        let lines = s.conform(json!({"title": ["First line", "second"], "price": 1}).as_object().unwrap(), None).unwrap();
        assert_eq!(lines["title"], "First line\nsecond");
        assert_eq!(s.conform(json!({"title": [1, 2], "price": 1}).as_object().unwrap(), None).unwrap_err().0, "title");
    }

    #[test]
    fn numbers_are_the_one_number_the_text_states() {
        let n = |v: Value| strict_number(&v);
        for (t, want) in [("$1,299.50", Some(1299.5)), ("721 points", Some(721.0)), ("169 m²", Some(169.0)), ("-5", Some(-5.0)), ("$-5", Some(-5.0)), ("39,595.52 NOK", Some(39595.52))] {
            assert_eq!(n(json!(t)), want, "{t}");
        }
        for t in ["call 555-1234", "2024-05-01", "range 10-20", "1.2k", "3M", "SKU-5", "n/a", "1,2"] {
            assert_eq!(n(json!(t)), None, "{t}");
        }
        assert_eq!(n(json!(true)), None);
        assert_eq!(number_json(1299.0), json!(1299));
        assert_eq!(number_json(1299.5), json!(1299.5));
    }

    #[test]
    fn enums_links_and_optional_fields_keep_their_meaning() {
        let s = schema(json!({"properties": {
            "stars": {"type": "number", "enum": [1, 2, 3]}, "link": {"type": "string", "format": "uri"}, "note": {"type": "string"}},
            "required": ["stars"]})).unwrap();
        let page = Some("https://example.com/a/b");
        let ok = s.conform(json!({"stars": "2 stars", "link": "/x"}).as_object().unwrap(), page).unwrap();
        assert_eq!(Value::Object(ok), json!({"stars": 2, "link": "https://example.com/x", "note": null}));
        assert_eq!(s.conform(json!({"stars": 2, "link": "N/A"}).as_object().unwrap(), page).unwrap_err().0, "link");
        assert_eq!(s.conform(json!({"stars": 2, "link": "  "}).as_object().unwrap(), page).unwrap()["link"], Value::Null);
        assert_eq!(s.raw["properties"]["note"]["type"], json!(["string", "null"]), "start.schema admits the nulls records carry");
        assert_eq!(s.raw["properties"]["stars"]["type"], json!("number"));
        assert!(schema(json!({"properties": {"x": {"type": "integer", "enum": ["a"]}}})).unwrap_err().contains("/enum"));
        assert!(schema(json!({"properties": {"x": {"type": "string", "description": 3}}})).unwrap_err().contains("/description"));
    }

    #[test]
    fn inferred_schemas_need_named_fields() {
        let p = fab_core::script::parse("for r in items \"rows\"\n  emit title: r.title, r.price\nend").unwrap();
        assert_eq!(inferred(&p), Some(json!({"type": "object", "properties": {"title": {}, "price": {}}})));
        let p = fab_core::script::parse("for r in items \"rows\"\n  emit r\nend").unwrap();
        assert_eq!(inferred(&p), None);
    }
}
