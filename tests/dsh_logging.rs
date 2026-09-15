//! Log defaults and overrides, exercised only against the in-process mock.

mod common;

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

use common::dsh::{
    active, base_handler, cli, code, idle, lines, notify, ok, start_mock, thread_value, Recorder,
    TempDir, Tx,
};

const THREAD_ID: &str = "T-logging";

fn complete_turn(tx: &Tx, turn_id: &str) {
    notify(
        tx,
        "thread/status",
        json!({ "threadId": THREAD_ID, "status": active() }),
    );
    notify(
        tx,
        "thread/event",
        json!({
            "threadId": THREAD_ID,
            "seq": 1, "type": "assistant/message", "data": { "content": [{"type": "text", "text": "all done"}] }
        }),
    );
    notify(
        tx,
        "turn/completed",
        json!({
            "threadId": THREAD_ID,
            "turnId": turn_id, "status": "completed", "reason": "completed", "finalMessage": "all done"
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
                ok(tx, msg, json!({ "turnId": "U1" }));
                complete_turn(tx, "U1");
            }
            "thread/resume" => {
                ok(tx, msg, thread_value(THREAD_ID, active(), json!([])));
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

#[test]
fn default_log_records_stdout_items_and_status_with_its_path() {
    let state = TempDir::new("logging-default");
    let output = run_command(logging_server(), &state)
        .output()
        .expect("running agent-bridge");
    assert_eq!(code(&output), 0, "{output:?}");

    let path = state.join("dsh/logs").join(format!("{THREAD_ID}.jsonl"));
    let events = lines(&output);
    assert_eq!(event_names(&events), ["started", "turn"]);
    for event in &events {
        assert_eq!(event["logPath"], json!(path));
    }

    let logged = read_log(&path);
    assert_eq!(event_names(&logged), ["started", "status", "item", "turn"]);
    assert_eq!(logged[0], events[0]);
    assert_eq!(logged[1]["status"], active());
    assert_eq!(logged[2]["itemType"], "assistant/message");
    assert_eq!(logged[2]["seq"], 1);
    assert_eq!(logged[2]["data"]["content"][0]["text"], "all done");
    assert_eq!(logged[3], events[1]);
}

#[test]
fn thread_event_payloads_and_request_resolution_are_logged_verbatim() {
    let state = TempDir::new("logging-protocol");
    let data =
        json!({"toolName": "shell", "arguments": {"command": "test"}, "nested": [1, null, true]});
    let expected = data.clone();
    let port = start_mock(base_handler(
        Recorder::default(),
        THREAD_ID,
        idle(),
        move |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
                notify(
                    tx,
                    "thread/event",
                    json!({"threadId": THREAD_ID, "seq": 42, "type": "tool/call", "data": data}),
                );
                notify(
                    tx,
                    "request/resolved",
                    json!({"threadId": THREAD_ID, "requestId": "req-42", "resolvedBy": "cancelled"}),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({"threadId": THREAD_ID, "turnId": "U1", "status": "failed", "reason": "max-tokens", "error": "context limit reached", "finalMessage": "partial answer"}),
                );
            }
        },
    ));
    let output = run_command(port, &state).output().unwrap();
    assert_eq!(code(&output), 1, "{output:?}");
    let logged = read_log(&state.join("dsh/logs").join(format!("{THREAD_ID}.jsonl")));
    assert_eq!(
        event_names(&logged),
        ["started", "item", "requestResolved", "turn"]
    );
    assert_eq!(logged[1]["itemType"], "tool/call");
    assert_eq!(logged[1]["seq"], 42);
    assert_eq!(logged[1]["data"], expected);
    assert_eq!(logged[2]["requestId"], "req-42");
    assert_eq!(logged[2]["resolvedBy"], "cancelled");
    let events = lines(&output);
    assert_eq!(events[1]["status"], "failed");
    assert_eq!(events[1]["error"], "context limit reached");
    assert_eq!(events[1]["reason"], "max-tokens");
    assert_eq!(events[1]["finalMessage"], "partial answer");
}
