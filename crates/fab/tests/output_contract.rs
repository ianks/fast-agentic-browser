//! Every command's stdout is the JSONL event stream of `fab_cli::events`:
//! `start` first, one `end` last, dense record `seq`s, and an exit code that
//! agrees with the `end`. These commands need no browser.

use fab_cli::events::{End, ErrorCode, Event};
use std::io::Write;
use std::process::{Command, ExitCode, Stdio};

struct Home(std::path::PathBuf);
impl Home {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("fab-contract-{}", fab_cli::task_store::TaskId::new().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs fab in an empty home; returns its events and exit code.
fn fab(home: &Home, args: &[&str], stdin: &str) -> (Vec<Event>, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fab"))
        .args(args)
        .env("FAB_HOME", &home.0)
        .env("FAB_CONFIG", home.0.join("config"))
        .env("FAB_MODEL", "none")
        .env_remove("FAB_SESSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let events = text.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not an event ({e}): {l}"))).collect();
    (events, out.status.code().unwrap())
}

/// The stream's invariants; returns its `end`.
fn conforms(cmd: &str, events: &[Event], code: i32) -> End {
    assert!(matches!(events.first(), Some(Event::Start { v: 1, cmd: c, .. }) if c == cmd), "{cmd}: starts with start: {events:?}");
    let Some(Event::End(end)) = events.last() else { panic!("{cmd}: ends with end: {events:?}") };
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Start { .. })).count(), 1);
    assert_eq!(events.iter().filter(|e| matches!(e, Event::End(_))).count(), 1);
    let seqs: Vec<u64> = events.iter().filter_map(|e| if let Event::Record { seq, .. } = e { Some(*seq) } else { None }).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{cmd}: dense seqs {seqs:?}");
    assert_eq!(end.ok, end.error.is_none(), "{cmd}: ok iff no error");
    assert_eq!(ExitCode::from(code as u8), end.exit_code(), "{cmd}: exit code follows the end");
    end.clone()
}

#[test]
fn browserless_commands_stream_one_contract() {
    let home = Home::new();
    let (events, code) = fab(&home, &["run", "--check", "-"], "set n = 1\nreturn n\n");
    assert!(conforms("run", &events, code).ok);

    let (events, code) = fab(&home, &["run", "-"], "for x in\n  emit x\n");
    let end = conforms("run", &events, code);
    assert_eq!(end.error.unwrap().code, ErrorCode::InvalidProgram);
    assert_eq!(code, 2);

    let (events, code) = fab(&home, &["do", "--print", "open example.com"], "");
    assert_eq!(conforms("do", &events, code).value, serde_json::json!("open example.com"));

    let (events, code) = fab(&home, &["tasks", "list"], "");
    assert!(conforms("tasks", &events, code).ok);

    let (events, code) = fab(&home, &["sessions"], "");
    assert!(conforms("sessions", &events, code).ok);

    let (events, code) = fab(&home, &["tasks", "show", "00000000000000000000000000000000"], "");
    assert!(!conforms("tasks", &events, code).ok);

    // Argument errors end the stream too (usage goes to stderr).
    for args in [&["frobnicate"][..], &["tasks", "show", "xyz"], &["--set", "nope=1", "sessions"]] {
        let (events, code) = fab(&home, args, "");
        let end = conforms(if args[0] == "--set" { "sessions" } else { "fab" }, &events, code);
        assert_eq!(end.error.unwrap().code, ErrorCode::InvalidArgs, "{args:?}");
    }

    let (events, code) = fab(&home, &["secrets", "reset", "--url", "example.com"], "");
    assert_eq!(conforms("secrets", &events, code).value, serde_json::json!({"site": "example.com", "cleared": 0}));
}
