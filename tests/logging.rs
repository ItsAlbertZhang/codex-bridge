//! These tests cover the shared core through the Codex mock and include Codex-specific cases.
//! Log defaults and overrides, exercised only against the in-process mock.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use serde_json::{json, Value};

use common::{
    active, base_handler, cli, code, idle, lines, notify, ok, start_mock, thread_value, Recorder,
    TempDir, Tx,
};

const THREAD_ID: &str = "T-logging";

fn complete_turn(tx: &Tx, turn_id: &str) {
    notify(
        tx,
        "thread/status/changed",
        json!({ "threadId": THREAD_ID, "status": active() }),
    );
    notify(
        tx,
        "item/completed",
        json!({
            "threadId": THREAD_ID,
            "turnId": turn_id,
            "item": { "id": "i1", "type": "agentMessage", "text": "all done" }
        }),
    );
    notify(
        tx,
        "turn/completed",
        json!({
            "threadId": THREAD_ID,
            "turn": { "id": turn_id, "status": "completed" }
        }),
    );
}

fn logging_server() -> u16 {
    start_mock(base_handler(
        Recorder::default(),
        THREAD_ID,
        idle(),
        |msg, tx, method| match method {
            "turn/start" => {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
                complete_turn(tx, "U1");
            }
            "thread/resume" => {
                ok(
                    tx,
                    msg,
                    json!({ "thread": thread_value(THREAD_ID, active(), json!([])) }),
                );
                complete_turn(tx, "U2");
            }
            _ => {}
        },
    ))
}

fn run_command(port: u16, state: &TempDir) -> Command {
    let mut command = cli(port, state);
    command.args(["run", "--cwd"]).arg(state.path()).args([
        "--prompt",
        "hi",
        "--timeout-secs",
        "5",
    ]);
    command
}

fn read_log(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("reading log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("log line must be JSON"))
        .collect()
}

fn event_names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().expect("event name"))
        .collect()
}

// Codex-specific: raw status objects and agentMessage item text in logs.
#[test]
fn default_log_records_stdout_items_and_status_with_its_path() {
    let state = TempDir::new("logging-default");
    let output = run_command(logging_server(), &state)
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "{output:?}");

    let path = state.join("codex/logs").join(format!("{THREAD_ID}.jsonl"));
    let events = lines(&output);
    assert_eq!(event_names(&events), ["started", "turn"]);
    for event in &events {
        assert_eq!(event["logPath"], json!(path));
    }

    let logged = read_log(&path);
    assert_eq!(event_names(&logged), ["started", "status", "item", "turn"]);
    assert_eq!(logged[0], events[0]);
    assert_eq!(logged[1]["status"], active());
    assert_eq!(logged[2]["text"], "all done");
    assert_eq!(logged[3], events[1]);
}

#[test]
fn no_log_creates_no_default_log_and_omits_log_path() {
    let state = TempDir::new("logging-disabled");
    let output = run_command(logging_server(), &state)
        .arg("--no-log")
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "{output:?}");

    let events = lines(&output);
    assert_eq!(event_names(&events), ["started", "turn"]);
    assert!(events.iter().all(|event| event.get("logPath").is_none()));
    assert!(!state.join("codex/logs").exists());
}

#[test]
fn explicit_log_appends_and_reports_the_requested_path() {
    let state = TempDir::new("logging-explicit");
    let path = state.join("custom.jsonl");
    let previous = json!({ "event": "previous", "text": "keep this line" });
    std::fs::write(&path, format!("{previous}\n")).expect("seeding explicit log");

    let output = run_command(logging_server(), &state)
        .arg("--log")
        .arg(&path)
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "{output:?}");

    let events = lines(&output);
    assert_eq!(event_names(&events), ["started", "turn"]);
    for event in &events {
        assert_eq!(event["logPath"], json!(path));
    }
    let logged = read_log(&path);
    assert_eq!(logged[0], previous, "--log must preserve existing contents");
    assert_eq!(
        event_names(&logged[1..]),
        ["started", "status", "item", "turn"]
    );
    assert_eq!(logged[1], events[0]);
    assert_eq!(logged[4], events[1]);
    assert!(!state.join("codex/logs").exists());
}

#[test]
fn wait_reuses_the_default_thread_log_without_overwriting_it() {
    let state = TempDir::new("logging-reuse");
    let port = logging_server();
    let first = run_command(port, &state)
        .output()
        .expect("running agent-bridge run");
    assert_eq!(code(&first), 0, "{first:?}");
    let path = state.join("codex/logs").join(format!("{THREAD_ID}.jsonl"));
    let before = read_log(&path);

    let output = cli(port, &state)
        .args(["wait", "--thread", THREAD_ID, "--timeout-secs", "5"])
        .output()
        .expect("running agent-bridge wait");
    assert_eq!(code(&output), 0, "{output:?}");

    let events = lines(&output);
    assert_eq!(event_names(&events), ["turn"]);
    assert_eq!(events[0]["turnId"], "U2");
    assert_eq!(events[0]["logPath"], json!(path));
    let logged = read_log(&path);
    assert_eq!(&logged[..before.len()], before.as_slice());
    assert_eq!(
        event_names(&logged[before.len()..]),
        ["status", "item", "turn"]
    );
    assert_eq!(logged.last(), events.last());
}

#[test]
fn a_failed_thread_start_does_not_create_a_default_log() {
    let state = TempDir::new("logging-no-thread");
    let port = start_mock(Arc::new(|msg, tx| {
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => ok(tx, msg, json!({ "userAgent": "mock/0" })),
            Some("thread/start") => {
                let _ = tx.send(json!({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "error": { "code": -32000, "message": "thread creation refused" }
                }));
            }
            _ => {}
        }
    }));

    let output = run_command(port, &state)
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 4, "{output:?}");
    let events = lines(&output);
    assert_eq!(event_names(&events), ["error"]);
    assert!(events[0].get("logPath").is_none());
    assert!(!state.join("codex/logs").exists());
}

// Codex-specific: status and agentMessage normalization also exercises shared log filtering.
#[test]
fn default_log_skips_events_received_before_the_thread_id_but_explicit_log_keeps_them() {
    let port = start_mock(Arc::new(|msg, tx| {
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => ok(tx, msg, json!({ "userAgent": "mock/0" })),
            Some("thread/start") => {
                notify(
                    tx,
                    "thread/status/changed",
                    json!({ "threadId": THREAD_ID, "status": idle() }),
                );
                notify(
                    tx,
                    "item/completed",
                    json!({
                        "threadId": THREAD_ID,
                        "turnId": "U0",
                        "item": {
                            "id": "before-thread-id",
                            "type": "agentMessage",
                            "text": "received before the thread id"
                        }
                    }),
                );
                ok(
                    tx,
                    msg,
                    json!({ "thread": thread_value(THREAD_ID, idle(), json!([])) }),
                );
            }
            Some("thread/read") => ok(
                tx,
                msg,
                json!({ "thread": thread_value(THREAD_ID, idle(), json!([])) }),
            ),
            Some("turn/start") => {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
                complete_turn(tx, "U1");
            }
            _ => {}
        }
    }));

    for explicit in [false, true] {
        let state = TempDir::new("logging-before-thread-id");
        let path = if explicit {
            state.join("explicit.jsonl")
        } else {
            state.join("codex/logs").join(format!("{THREAD_ID}.jsonl"))
        };
        let mut command = run_command(port, &state);
        if explicit {
            command.arg("--log").arg(&path);
        }
        let output = command.output().expect("running agent-bridge");
        assert_eq!(code(&output), 0, "explicit={explicit}: {output:?}");
        let events = lines(&output);
        assert_eq!(event_names(&events), ["started", "turn"]);
        assert_eq!(events[1]["finalMessage"], "all done");
        for event in &events {
            assert_eq!(event["logPath"], json!(path));
        }

        let logged = read_log(&path);
        let expected = if explicit {
            vec!["started", "status", "item", "status", "item", "turn"]
        } else {
            vec!["started", "status", "item", "turn"]
        };
        assert_eq!(event_names(&logged), expected, "explicit={explicit}");
        let old_item = logged
            .iter()
            .any(|event| event["text"] == "received before the thread id");
        let old_status = logged
            .iter()
            .any(|event| event["event"] == "status" && event["status"] == idle());
        assert_eq!(old_item, explicit, "explicit={explicit}");
        assert_eq!(old_status, explicit, "explicit={explicit}");
        assert_eq!(logged[0], events[0]);
        assert_eq!(logged[logged.len() - 3]["status"], active());
        assert_eq!(logged[logged.len() - 2]["text"], "all done");
        assert_eq!(logged.last(), events.last());
    }
}

#[test]
fn no_log_conflicts_with_an_explicit_log_before_connecting() {
    let state = TempDir::new("logging-conflict");
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        THREAD_ID,
        idle(),
        |_, _, _| {},
    ));
    let path = state.join("conflict.jsonl");
    let output = run_command(port, &state)
        .arg("--log")
        .arg(&path)
        .arg("--no-log")
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 4, "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--log"), "{stderr}");
    assert!(stderr.contains("--no-log"), "{stderr}");
    assert!(
        recorder.all().is_empty(),
        "invalid arguments must not connect"
    );
    assert!(!path.exists());
    assert!(!state.join("codex/logs").exists());
}
