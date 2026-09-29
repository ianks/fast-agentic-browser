//! The LLM-facing tool surface of the planner (agent mode and the bench arms).

use serde_json::{Value, json};
use std::fmt::Write;
use fab_core::{ActResult, Session, Timings};

/// The goal-level tool (the experiment-goal arm).
pub fn do_definition() -> Value {
    json!({
        "name": "do",
        "description": "Accomplish a goal on the website end to end: navigation, forms, dialogs, retries, re-login and verification are handled for you, and every part of the goal is checked before it reports done. Pass the goal in words. If the goal needs reading, counting, summing or comparing data across rows or pages, also pass `steps`: instruction strings and data ops, e.g. [\"open the Orders page\", {\"collect\": \"orders\", \"where\": \"status is Refunded and customer is <name>\", \"op\": \"sum\", \"of\": \"total\", \"as\": \"refunds\"}]. Use {\"collect\": ..., \"op\": \"argmax\", \"by\": \"<column>\", \"as\": \"x\"} to find the top group, then \"{x}\" in later instruction strings. Returns status (done / incomplete / failed / ambiguous), results, verbatim evidence, and the actions taken.",
        "inputSchema": {"type": "object", "properties": {
            "goal": {"type": "string"},
            "steps": {"type": "array", "items": {}}
        }, "required": ["goal"]}
    })
}

/// Fills `{name}` holes of an answer template from `do` results; None while
/// any hole is still unfilled.
pub fn fill_answer(template: &str, results: &Value) -> Option<String> {
    let mut out = template.to_string();
    if let Some(m) = results.as_object() {
        for (k, v) in m {
            let val = match v {
                Value::String(s) => s.clone(),
                Value::Object(o) if o.contains_key("top") => match &o["top"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
                other => other.to_string(),
            };
            out = out.replace(&format!("{{{k}}}"), &val);
        }
    }
    let unfilled = out.find('{').is_some_and(|a| out[a..].contains('}'));
    (!unfilled).then_some(out)
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "goto",
            "description": "Open a URL. Returns a compact summary of the page.",
            "inputSchema": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]}
        }),
        json!({
            "name": "act",
            "description": "Carry out one instruction on the current page. Precise commands using element ids from the page listing run instantly (e.g. click e<id>; type \"<text>\" into e<id>; select \"<option>\" from e<id>; check e<id>; press enter; several joined with commas). High-level instructions also work (e.g. log in as \"<username>\" with password \"<password>\"; add the <product> in size <size> to the cart) and may take several clicks. Put literal text in double quotes. Returns what was done and the resulting page.",
            "inputSchema": {"type": "object", "properties": {"instruction": {"type": "string"}}, "required": ["instruction"]}
        }),
        json!({
            "name": "run",
            "description": "Carry out several instructions in order in a single call (no round-trip between them). Stops at the first failure. Use when you can predict the next steps.",
            "inputSchema": {"type": "object", "properties": {"instructions": {"type": "array", "items": {"type": "string"}}}, "required": ["instructions"]}
        }),
        json!({
            "name": "read",
            "description": "Return the current page's text (tables as `a | b | c` rows). Use it to read data. For long pages pass a query to keep the most relevant parts.",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}
        }),
        json!({
            "name": "collect",
            "description": "Read a table, list or feed on the current page across ALL its pages and compute over it exactly (in code): counts, sums, max/min, top groups. Use this instead of reading and adding up rows yourself. Returns the result plus the matching rows verbatim.",
            "inputSchema": {"type": "object", "properties": {
                "what": {"type": "string", "description": "what the rows are, e.g. \"orders\""},
                "where": {"type": "string", "description": "which rows count, in words, e.g. \"status is Refunded and customer is Linnea Dahlqvist\"; omit for all rows"},
                "op": {"type": "string", "enum": ["count", "sum", "max", "min", "argmax", "argmin", "list"]},
                "of": {"type": "string", "description": "the quantity to sum/compare, e.g. \"total\""},
                "by": {"type": "string", "description": "group by this, e.g. \"account\" (with count/sum: returns the top group)"},
                "pages": {"type": "string", "enum": ["all", "current"], "description": "default all"}
            }, "required": ["what", "op"]}
        }),
        json!({
            "name": "extract",
            "description": "Answer a question from the current page's text. Returns the verbatim text block that answers it.",
            "inputSchema": {"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]}
        }),
        json!({
            "name": "observe",
            "description": "List the interactive elements most relevant to a query (or all, if empty).",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}
        }),
    ]
}

fn fmt_act(out: &mut String, r: &ActResult) {
    let _ = writeln!(
        out,
        "{} · {} step(s) · {:.0} ms · {}",
        if r.ok { "ok" } else { "FAILED" },
        r.steps,
        r.timings.total_ms,
        r.instruction
    );
    for a in &r.actions {
        let _ = writeln!(out, "- {a}");
    }
    if let Some(e) = &r.error {
        let _ = writeln!(out, "error: {e}");
    }
    if !r.alternatives.is_empty() {
        let alts: Vec<String> = r.alternatives.iter().map(|(k, p)| format!("{k} ({p:.2})")).collect();
        let _ = writeln!(out, "candidates: {}", alts.join(", "));
    }
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    /// Input may have reached the page before the failure: the outcome is
    /// unknown and the call must not be repeated automatically.
    Uncertain,
}

impl Status {
    fn of(ok: bool) -> Self {
        if ok { Status::Ok } else { Status::Failed }
    }
    fn after(ok: bool, uncertain: bool) -> Self {
        if uncertain { Status::Uncertain } else { Status::of(ok) }
    }
}

/// Runs a tool; returns (text for the model, status, timings).
pub async fn call(sess: &mut Session, name: &str, args: &Value) -> (String, Status, Option<Timings>) {
    let (text, ok, uncertain, timings) = run_tool(sess, name, args).await;
    (text, Status::after(ok, uncertain), timings)
}

async fn run_tool(sess: &mut Session, name: &str, args: &Value) -> (String, bool, bool, Option<Timings>) {
    let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    let (text, ok, timings) = match name {
        "goto" => match sess.goto(&s("url")).await {
            Ok(p) => (p, true, None),
            Err(e) => (format!("error: {e:#}"), false, None),
        },
        "act" => {
            let before = sess.last_snapshot().cloned();
            let r = sess.act(&s("instruction")).await;
            let mut out = String::new();
            fmt_act(&mut out, &r);
            out.push('\n');
            out.push_str(&sess.page_after(before.as_ref()).await.unwrap_or_default());
            return (out, r.ok, r.uncertain, Some(r.timings));
        }
        "run" => {
            let list: Vec<String> = args["instructions"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let before = sess.last_snapshot().cloned();
            let mut out = String::new();
            let mut tm = Timings::default();
            let mut done = 0;
            let mut ok = true;
            let mut uncertain = false;
            for ins in &list {
                // `read [query]` inside a run returns page text instead of acting.
                let l = ins.trim().to_lowercase();
                if l == "read" || l.starts_with("read ") || l.starts_with("read(") {
                    let q = ins.trim()[4..].trim().trim_matches(|c| c == '(' || c == ')' || c == '"');
                    let text = sess.read(q, 6000).await.unwrap_or_else(|e| format!("error: {e:#}"));
                    let _ = writeln!(out, "read {q}\n{text}");
                    done += 1;
                    continue;
                }
                let r = sess.act(ins).await;
                fmt_act(&mut out, &r);
                tm.merge(&r.timings);
                done += 1;
                if !r.ok {
                    ok = false;
                    uncertain = r.uncertain;
                    break;
                }
            }
            if done < list.len() {
                let _ = writeln!(out, "(stopped; {} instruction(s) not run)", list.len() - done);
            }
            out.push('\n');
            out.push_str(&sess.page_after(before.as_ref()).await.unwrap_or_default());
            return (out, ok, uncertain, Some(tm));
        }
        "extract" => match sess.extract(&s("question")).await {
            Ok(r) => {
                let text = json!({"answer": r.answer, "p": (r.p * 100.0).round() / 100.0, "exists": (r.exists * 100.0).round() / 100.0});
                (text.to_string(), r.answer.is_some(), Some(r.timings))
            }
            Err(e) => (format!("error: {e:#}"), false, None),
        },
        "do" => {
            let steps: Vec<Value> = args["steps"].as_array().cloned().unwrap_or_default();
            let before = sess.last_snapshot().cloned();
            let mut v = sess.do_goal(&s("goal"), &steps).await;
            if let (Some(t), Some("done")) = (args["answer"].as_str().filter(|t| !t.is_empty()), v["status"].as_str()) {
                if let Some(a) = fill_answer(t, &v["results"]) {
                    v["final_answer"] = json!(a);
                }
            }
            let ok = matches!(v["status"].as_str(), Some("done" | "steps_done"));
            let mut out = v.to_string();
            out.push('\n');
            out.push_str(&sess.page_after(before.as_ref()).await.unwrap_or_default());
            let tm: Option<Timings> = serde_json::from_value(v["timings"].clone()).ok();
            return (out, ok, v["uncertain"] == json!(true), tm);
        }
        "collect" => {
            let opt = |k: &str| args[k].as_str().filter(|v| !v.is_empty()).map(str::to_string);
            let all = args["pages"].as_str() != Some("current");
            match sess.collect(&s("what"), &s("where"), all, &s("op"), opt("of").as_deref(), opt("by").as_deref()).await {
                Ok(mut v) => {
                    // Only a result that matched rows can end the task by itself.
                    let matched = v["matched"].as_u64().unwrap_or(1) > 0;
                    if let (Some(t), true) = (opt("answer"), matched) {
                        if let Some(a) = fill_answer(&t, &json!({"result": v["result"].clone()})) {
                            v["final_answer"] = json!(a);
                        }
                    }
                    (v.to_string(), true, None)
                }
                Err(e) => (format!("error: {e:#}"), false, None),
            }
        }
        "read" => match sess.read(&s("query"), 8000).await {
            Ok(t) => (t, true, None),
            Err(e) => (format!("error: {e:#}"), false, None),
        },
        "observe" => match sess.observe(&s("query")).await {
            Ok(t) => (t, true, None),
            Err(e) => (format!("error: {e:#}"), false, None),
        },
        other => (format!("unknown tool {other}"), false, None),
    };
    (text, ok, false, timings)
}
