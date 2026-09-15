//! These tests cover the shared core through the Codex mock and include Codex-specific cases.
//! Integration tests against an in-process mock app-server.
//!
//! Each test binds a mock on an ephemeral port (it also answers `/readyz`, so
//! the daemon check passes without spawning anything), runs the real
//! `agent-bridge` binary against it, and asserts on the JSON lines it prints
//! and on the exit code.

mod common;

use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::{
    active, base_handler, cli, code, dead_port, idle, lines, notify, ok, server_request,
    start_mock, thread_value, Handler, Recorder, TempDir, Tx,
};

fn run(port: u16, args: &[&str]) -> Output {
    let state = TempDir::new("session");
    cli(port, &state)
        .args(args)
        .output()
        .expect("running agent-bridge")
}

// Codex-specific: numeric request IDs and the initialized notification.
#[test]
fn handshake_precedes_every_request() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            }
        },
    ));

    let output = run(
        port,
        &["run", "--cwd", "/tmp", "--prompt", "hi", "--no-wait"],
    );
    assert_eq!(code(&output), 0);

    let seen = recorder.all();
    assert_eq!(seen[0]["method"], "initialize");
    assert_eq!(seen[0]["params"]["clientInfo"]["name"], "agent-bridge");
    assert!(seen[0]["id"].is_number(), "initialize must be a request");
    assert_eq!(seen[1]["method"], "initialized");
    assert!(
        seen[1].get("id").is_none(),
        "initialized must be a notification"
    );

    let started = &lines(&output)[0];
    assert_eq!(started["event"], "started");
    assert_eq!(started["threadId"], "T1");
    assert_eq!(started["turnId"], "U1");
}

#[test]
fn developer_instructions_are_read_once_before_connecting() {
    let state = TempDir::new("instructions-before-connect");
    let instructions = state.join("instructions.txt");
    let original = "Keep the API stable.\nPreserve the original instructions.\n";
    std::fs::write(&instructions, original).expect("initial instructions");
    let removed_on_handshake = instructions.clone();
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => {
                std::fs::remove_file(&removed_on_handshake)
                    .expect("remove instructions at handshake");
                ok(tx, msg, json!({ "userAgent": "mock/0" }));
            }
            "thread/start" | "thread/read" => ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", idle(), json!([])) }),
            ),
            "turn/start" => ok(tx, msg, json!({ "turn": { "id": "U1" } })),
            _ => {}
        }
    }));

    let output = cli(port, &state)
        .args(["run", "--cwd"])
        .arg(state.path())
        .args(["--prompt", "hi", "--developer-instructions-file"])
        .arg(&instructions)
        .arg("--no-wait")
        .output()
        .expect("running with instructions removed during handshake");

    assert_eq!(code(&output), 0, "{output:?}");
    assert!(
        !instructions.exists(),
        "the handshake removed the source file"
    );
    assert_eq!(
        recorder.requests("thread/start")[0]["params"]["developerInstructions"],
        original
    );
    assert_eq!(lines(&output)[0]["event"], "started");
}

// Codex-specific: agentMessage items, durationMs, and turn-level effort/input shape.
#[test]
fn run_waits_for_the_main_turn_to_complete() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method != "turn/start" {
                return;
            }
            ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            notify(
                tx,
                "item/agentMessage/delta",
                json!({ "threadId": "T1", "delta": "ignored" }),
            );
            notify(
                tx,
                "item/completed",
                json!({
                    "threadId": "T1",
                    "turnId": "U1",
                    "completedAtMs": 1,
                    "item": { "id": "i1", "type": "agentMessage", "text": "all done" }
                }),
            );
            notify(
                tx,
                "turn/completed",
                json!({
                    "threadId": "T1",
                    "turn": { "id": "U1", "status": "completed", "durationMs": 1234 }
                }),
            );
        },
    ));

    let output = run(
        port,
        &["run", "--cwd", "/tmp", "--prompt", "hi", "--effort", "low"],
    );
    assert_eq!(code(&output), 0);

    let lines = lines(&output);
    assert_eq!(lines.len(), 2, "started + turn, deltas are ignored");
    let turn = &lines[1];
    assert_eq!(turn["event"], "turn");
    assert_eq!(turn["status"], "completed");
    assert_eq!(turn["threadId"], "T1");
    assert_eq!(turn["turnId"], "U1");
    assert_eq!(turn["finalMessage"], "all done");
    assert_eq!(turn["durationMs"], 1234);
    assert!(turn.get("reason").is_none());

    let turn_start = &recorder.requests("turn/start")[0];
    assert_eq!(turn_start["params"]["effort"], "low");
    assert_eq!(turn_start["params"]["input"][0]["type"], "text");
}

// Codex-specific: sub-agent discovery and thread/unsubscribe.
#[test]
fn a_sub_agent_turn_does_not_end_the_wait() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method != "turn/start" {
                return;
            }
            ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            notify(
                tx,
                "item/completed",
                json!({
                    "threadId": "T1",
                    "turnId": "U1",
                    "completedAtMs": 1,
                    "item": {
                        "id": "i1",
                        "type": "subAgentActivity",
                        "kind": "started",
                        "agentThreadId": "SUB",
                        "agentPath": "reviewer"
                    }
                }),
            );
            // The sub-agent's own lifecycle, broadcast on the same connection.
            notify(
                tx,
                "turn/started",
                json!({ "threadId": "SUB", "turn": { "id": "SU1", "status": "inProgress" } }),
            );
            notify(
                tx,
                "turn/completed",
                json!({ "threadId": "SUB", "turn": { "id": "SU1", "status": "failed" } }),
            );
            notify(
                tx,
                "item/completed",
                json!({
                    "threadId": "T1",
                    "turnId": "U1",
                    "completedAtMs": 2,
                    "item": { "id": "i2", "type": "agentMessage", "text": "main answer" }
                }),
            );
            notify(
                tx,
                "turn/completed",
                json!({ "threadId": "T1", "turn": { "id": "U1", "status": "completed" } }),
            );
        },
    ));

    let output = run(port, &["run", "--cwd", "/tmp", "--prompt", "hi"]);
    assert_eq!(code(&output), 0, "the sub-agent failure must not leak out");

    let lines = lines(&output);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["status"], "completed");
    assert_eq!(lines[1]["finalMessage"], "main answer");

    let unsubscribes = recorder.requests("thread/unsubscribe");
    assert_eq!(unsubscribes.len(), 1);
    assert_eq!(unsubscribes[0]["params"]["threadId"], "SUB");
}

// Codex-specific: command approval method and numeric request ID.
#[test]
fn a_server_request_exits_two_with_verbatim_params() {
    let recorder = Recorder::default();
    let params = json!({
        "threadId": "T1",
        "turnId": "U1",
        "itemId": "i9",
        "startedAtMs": 42,
        "command": ["rm", "-rf", "/"],
        "cwd": "/tmp",
        "reason": "destructive"
    });
    let expected = params.clone();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        move |msg, tx, method| {
            if method != "turn/start" {
                return;
            }
            ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            server_request(
                tx,
                55,
                "item/commandExecution/requestApproval",
                params.clone(),
            );
        },
    ));

    let output = run(port, &["run", "--cwd", "/tmp", "--prompt", "hi"]);
    assert_eq!(code(&output), 2);

    let lines = lines(&output);
    let request = &lines[1];
    assert_eq!(request["event"], "request");
    assert_eq!(request["threadId"], "T1");
    assert_eq!(request["requestId"], 55);
    assert_eq!(request["method"], "item/commandExecution/requestApproval");
    assert_eq!(request["params"], expected);

    // The request must stay pending on the server.
    assert!(
        !recorder
            .all()
            .iter()
            .any(|m| m.get("id") == Some(&json!(55))),
        "agent-bridge must not answer the request it handed over"
    );
}

// Codex-specific: approval result uses decision accept.
#[test]
fn reply_answers_the_redelivered_request_with_a_schema_shaped_decision() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| {
            match method {
                "thread/resume" => {
                    ok(
                        tx,
                        msg,
                        json!({ "thread": thread_value("T1", active(), json!([])) }),
                    );
                    // Codex re-sends the pending request with the same id.
                    server_request(
                        tx,
                        77,
                        "item/commandExecution/requestApproval",
                        json!({ "threadId": "T1", "turnId": "U1", "itemId": "i1", "startedAtMs": 1 }),
                    );
                }
                _ => {
                    // The decision arrives as a JSON-RPC response, not a request.
                    if msg.get("id") == Some(&json!(77)) {
                        notify(
                            tx,
                            "item/completed",
                            json!({
                                "threadId": "T1",
                                "turnId": "U1",
                                "completedAtMs": 3,
                                "item": { "id": "i2", "type": "agentMessage", "text": "approved and done" }
                            }),
                        );
                        notify(
                            tx,
                            "turn/completed",
                            json!({ "threadId": "T1", "turn": { "id": "U1", "status": "completed" } }),
                        );
                    }
                }
            }
        },
    ));

    let output = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "77",
            "--decision",
            "accept",
        ],
    );
    assert_eq!(code(&output), 0);

    let answer = recorder
        .all()
        .into_iter()
        .find(|m| m.get("id") == Some(&json!(77)))
        .expect("the decision must reach the server");
    assert_eq!(answer["result"], json!({ "decision": "accept" }));

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "turn");
    assert_eq!(lines[0]["finalMessage"], "approved and done");
}

// Codex-specific: numeric and string request IDs compare equal during shared collection.
#[test]
fn reply_without_an_id_waits_for_the_full_window_and_deduplicates_requests() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| match method {
            "thread/resume" => {
                ok(
                    tx,
                    msg,
                    json!({ "thread": thread_value("T1", active(), json!([])) }),
                );
                let _ = tx.send(json!({
                    "jsonrpc": "2.0",
                    "id": "77",
                    "method": "item/commandExecution/requestApproval",
                    "params": { "threadId": "T1", "turnId": "U1" },
                }));
                server_request(
                    tx,
                    77,
                    "item/commandExecution/requestApproval",
                    json!({ "threadId": "T1", "turnId": "U1" }),
                );
                server_request(
                    tx,
                    78,
                    "item/fileChange/requestApproval",
                    json!({ "threadId": "SUB", "turnId": "SU1" }),
                );
                notify(
                    tx,
                    "item/completed",
                    json!({
                        "threadId": "T1",
                        "turnId": "U1",
                        "item": { "type": "agentMessage", "text": "collected before replying" },
                    }),
                );
            }
            _ if msg.get("id") == Some(&json!("77")) => notify(
                tx,
                "turn/completed",
                json!({ "threadId": "T1", "turn": { "id": "U1", "status": "completed" } }),
            ),
            _ => {}
        },
    ));

    let started = Instant::now();
    let output = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--decision",
            "accept",
            "--stall-secs",
            "1",
            "--timeout-secs",
            "2",
        ],
    );
    assert_eq!(code(&output), 0, "{:?}", lines(&output));
    assert!(started.elapsed() >= Duration::from_secs(10));
    let answers: Vec<Value> = recorder
        .all()
        .into_iter()
        .filter(|msg| msg.get("method").is_none() && msg.get("id").is_some())
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "duplicate delivery must receive one answer"
    );
    assert_eq!(answers[0]["id"], "77");
    assert_eq!(answers[0]["result"], json!({ "decision": "accept" }));
    assert_eq!(
        lines(&output)[0]["finalMessage"],
        "collected before replying"
    );
}

#[test]
fn reply_without_an_id_refuses_multiple_requests_without_answering_any() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| {
            if method != "thread/resume" {
                return;
            }
            ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", active(), json!([])) }),
            );
            server_request(
                tx,
                77,
                "item/commandExecution/requestApproval",
                json!({ "threadId": "T1", "turnId": "U1" }),
            );
            // A later delivery must still prevent replying to the first one.
            let tx = tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                server_request(
                    &tx,
                    78,
                    "item/fileChange/requestApproval",
                    json!({ "threadId": "T1", "turnId": "U1" }),
                );
            });
        },
    ));

    let started = Instant::now();
    let output = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--decision",
            "accept",
            "--auto-decline",
            "--follow",
            "--stall-secs",
            "1",
            "--timeout-secs",
            "1",
        ],
    );
    assert_eq!(code(&output), 4);
    assert!(started.elapsed() >= Duration::from_secs(10));
    let events = lines(&output);
    assert_eq!(
        events.len(),
        1,
        "wait options must not interrupt collection"
    );
    assert_eq!(events[0]["event"], "error");
    assert!(events[0]["message"]
        .as_str()
        .unwrap()
        .contains("--request-id"));
    assert!(
        recorder.all().iter().all(|msg| msg.get("method").is_some()),
        "an ambiguous reply must neither answer nor auto-decline any request"
    );
}

#[test]
fn reply_without_an_id_ignores_other_threads_even_if_the_turn_ended() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "thread/resume" {
                ok(
                    tx,
                    msg,
                    json!({ "thread": thread_value("T1", idle(), json!([])) }),
                );
                server_request(
                    tx,
                    99,
                    "item/commandExecution/requestApproval",
                    json!({ "threadId": "SUB", "turnId": "SU1" }),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({ "threadId": "T1", "turn": { "id": "U1", "status": "completed" } }),
                );
            }
        },
    ));

    let started = Instant::now();
    let output = run(port, &["reply", "--thread", "T1", "--decision", "accept"]);
    assert_eq!(code(&output), 4);
    assert!(started.elapsed() >= Duration::from_secs(10));
    let events = lines(&output);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "error");
    assert!(events[0]["message"]
        .as_str()
        .unwrap()
        .contains("no pending server request"));
    assert!(recorder.all().iter().all(|msg| msg.get("method").is_some()));
}

// Codex-specific: inProgress turns and input text blocks.
#[test]
fn steer_without_a_turn_uses_the_latest_turn_in_progress() {
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/read" => ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", active(), json!([
                    { "id": "U1", "status": "inProgress" },
                    { "id": "U2", "status": "inProgress" },
                    { "id": "U3", "status": "completed" },
                ])) }),
            ),
            "turn/steer" => ok(tx, msg, json!({})),
            _ => {}
        }
    }));

    let output = run(
        port,
        &["steer", "--thread", "T1", "--text", "change course"],
    );
    assert_eq!(code(&output), 0);
    assert_eq!(
        recorder.requests("thread/read")[0]["params"]["includeTurns"],
        true
    );
    let requests = recorder.requests("turn/steer");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["expectedTurnId"], "U2");
    assert_eq!(requests[0]["params"]["input"][0]["text"], "change course");
    assert_eq!(lines(&output)[0]["event"], "steered");
}

#[test]
fn steer_without_a_turn_refuses_an_idle_thread() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/steer" {
                ok(tx, msg, json!({}));
            }
        },
    ));

    let output = run(
        port,
        &["steer", "--thread", "T1", "--text", "change course"],
    );
    assert_eq!(code(&output), 4);
    assert!(recorder.requests("turn/steer").is_empty());
    assert!(lines(&output)[0]["message"]
        .as_str()
        .unwrap()
        .contains("no turn in progress to steer"));
}

// Codex-specific: explicit expectedTurnId remains a string in the input-block steer request.
#[test]
fn steer_with_a_turn_keeps_the_explicit_expected_id() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/steer" {
                ok(tx, msg, json!({}));
            }
        },
    ));

    let output = run(
        port,
        &[
            "steer",
            "--thread",
            "T1",
            "--turn",
            "chosen",
            "--text",
            "change course",
        ],
    );
    assert_eq!(code(&output), 0);
    assert!(recorder.requests("thread/read").is_empty());
    assert_eq!(
        recorder.requests("turn/steer")[0]["params"]["expectedTurnId"],
        "chosen"
    );
}

#[test]
fn run_is_refused_while_the_thread_is_active() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            }
        },
    ));

    let output = run(port, &["run", "--cwd", "/tmp", "--prompt", "hi"]);
    assert_eq!(code(&output), 4);

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "error");
    let message = lines[0]["message"].as_str().unwrap();
    assert!(message.contains("already running"), "got: {message}");
    assert!(
        message.contains("active"),
        "the original status is included: {message}"
    );

    assert!(
        recorder.requests("turn/start").is_empty(),
        "the prompt must not be folded into the running turn"
    );
}

#[test]
fn a_silent_turn_stalls() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
                // ... and then nothing at all.
            }
        },
    ));

    let output = run(
        port,
        &[
            "run",
            "--cwd",
            "/tmp",
            "--prompt",
            "hi",
            "--stall-secs",
            "1",
        ],
    );
    assert_eq!(code(&output), 3);

    let lines = lines(&output);
    assert_eq!(lines[1]["event"], "stalled");
    assert_eq!(lines[1]["threadId"], "T1");
    assert_eq!(lines[1]["turnId"], "U1");
    assert_eq!(lines[1]["stallSecs"], 1);
}

// Codex-specific: historical agentMessage extraction.
#[test]
fn wait_on_an_idle_thread_reports_the_last_turn() {
    let turns = json!([
        { "id": "U1", "status": "completed", "items": [{ "type": "agentMessage", "text": "older" }] },
        { "id": "U2", "status": "failed", "items": [{ "type": "agentMessage", "text": "newest" }] }
    ]);
    let recorder = Recorder::default();
    let inner = recorder.clone();
    let handler: Handler = Arc::new(move |msg: &Value, tx: &Tx| {
        inner.push(msg.clone());
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/resume" => ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", idle(), json!([])) }),
            ),
            "thread/read" => ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", idle(), turns.clone()) }),
            ),
            _ => {}
        }
    });
    let port = start_mock(handler);

    let output = run(port, &["wait", "--thread", "T1"]);
    assert_eq!(code(&output), 1, "the last turn failed");

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "turn");
    assert_eq!(lines[0]["status"], "failed");
    assert_eq!(lines[0]["turnId"], "U2");
    assert_eq!(lines[0]["finalMessage"], "newest");

    let read = &recorder.requests("thread/read")[0];
    assert_eq!(read["params"]["includeTurns"], true);
}

#[test]
fn auto_decline_answers_requests_and_keeps_waiting() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| match method {
            "turn/start" => {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
                server_request(
                    tx,
                    9,
                    "item/fileChange/requestApproval",
                    json!({ "threadId": "T1", "turnId": "U1", "itemId": "i1", "startedAtMs": 1 }),
                );
            }
            _ => {
                if msg.get("id") == Some(&json!(9)) {
                    notify(
                        tx,
                        "turn/completed",
                        json!({ "threadId": "T1", "turn": { "id": "U1", "status": "completed" } }),
                    );
                }
            }
        },
    ));

    let output = run(
        port,
        &["run", "--cwd", "/tmp", "--prompt", "hi", "--auto-decline"],
    );
    assert_eq!(code(&output), 0);
    let lines = lines(&output);
    assert_eq!(lines.len(), 2, "the declined request is not a stdout event");
    assert_eq!(lines[1]["event"], "turn");

    let answer = recorder
        .all()
        .into_iter()
        .find(|m| m.get("id") == Some(&json!(9)))
        .expect("the decline must reach the server");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("agent-bridge is running this turn unattended:"),
        "got: {message}"
    );
}

// Codex-specific: status.type and normalized Codex history.
#[test]
fn status_and_read_print_their_own_shapes() {
    let turns = json!([
        { "id": "U1", "status": "completed", "items": [{ "type": "agentMessage", "text": "hello" }] }
    ]);
    let handler: Handler = Arc::new(move |msg: &Value, tx: &Tx| {
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/read" => {
                let include = msg["params"]["includeTurns"].as_bool().unwrap_or(false);
                let turns = if include { turns.clone() } else { json!([]) };
                ok(
                    tx,
                    msg,
                    json!({ "thread": thread_value("T1", idle(), turns) }),
                );
            }
            _ => {}
        }
    });
    let port = start_mock(handler);

    let status = run(port, &["status", "--thread", "T1"]);
    assert_eq!(code(&status), 0);
    let status = &lines(&status)[0];
    assert_eq!(status["threadId"], "T1");
    assert_eq!(status["status"]["type"], "idle");
    assert_eq!(status["cwd"], "/tmp");
    assert_eq!(status["model"], "mock-model");

    let read = run(port, &["read", "--thread", "T1"]);
    assert_eq!(code(&read), 0);
    let read = &lines(&read)[0];
    assert_eq!(
        read["turns"],
        json!([{ "id": "U1", "status": "completed" }])
    );
    assert_eq!(read["finalMessage"], "hello");
}

#[test]
fn follow_prints_the_request_and_keeps_going() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method != "turn/start" {
                return;
            }
            ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            server_request(
                tx,
                31,
                "item/commandExecution/requestApproval",
                json!({ "threadId": "T1", "turnId": "U1", "itemId": "i1", "startedAtMs": 1 }),
            );
            notify(
                tx,
                "turn/completed",
                json!({ "threadId": "T1", "turn": { "id": "U1", "status": "interrupted" } }),
            );
        },
    ));

    let output = run(
        port,
        &["run", "--cwd", "/tmp", "--prompt", "hi", "--follow"],
    );
    assert_eq!(code(&output), 1, "interrupted still ends the process");

    let lines = lines(&output);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["event"], "started");
    assert_eq!(lines[1]["event"], "request");
    assert_eq!(lines[1]["requestId"], 31);
    assert_eq!(lines[2]["event"], "turn");
    assert_eq!(lines[2]["status"], "interrupted");

    assert!(
        !recorder
            .all()
            .iter()
            .any(|m| m.get("id") == Some(&json!(31))),
        "--follow must leave the request pending too"
    );
}

// Codex-specific: resumed-turn input blocks and agentMessage extraction.
#[test]
fn run_on_an_existing_idle_thread_starts_a_turn_without_opening_one() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| match method {
            "thread/resume" => ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", idle(), json!([])) }),
            ),
            "turn/start" => {
                ok(tx, msg, json!({ "turn": { "id": "U2" } }));
                notify(
                    tx,
                    "item/completed",
                    json!({
                        "threadId": "T1",
                        "turnId": "U2",
                        "completedAtMs": 1,
                        "item": { "id": "i1", "type": "agentMessage", "text": "second answer" }
                    }),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({ "threadId": "T1", "turn": { "id": "U2", "status": "completed" } }),
                );
            }
            _ => {}
        },
    ));

    let output = run(port, &["run", "--thread", "T1", "--prompt", "again"]);
    assert_eq!(code(&output), 0);

    assert!(
        recorder.requests("thread/start").is_empty(),
        "--thread must reuse the thread, not open a new one"
    );
    let resumes = recorder.requests("thread/resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["params"]["threadId"], "T1");
    let turn_start = &recorder.requests("turn/start")[0];
    assert_eq!(turn_start["params"]["threadId"], "T1");
    assert_eq!(turn_start["params"]["input"][0]["text"], "again");

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "started");
    assert_eq!(lines[0]["threadId"], "T1");
    assert_eq!(lines[0]["turnId"], "U2");
    assert_eq!(lines[1]["event"], "turn");
    assert_eq!(lines[1]["status"], "completed");
    assert_eq!(lines[1]["finalMessage"], "second answer");
}

#[test]
fn run_on_an_existing_active_thread_is_refused() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| match method {
            "thread/resume" => ok(
                tx,
                msg,
                json!({ "thread": thread_value("T1", active(), json!([])) }),
            ),
            "turn/start" => ok(tx, msg, json!({ "turn": { "id": "U2" } })),
            _ => {}
        },
    ));

    let output = run(port, &["run", "--thread", "T1", "--prompt", "again"]);
    assert_eq!(code(&output), 4);

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "error");
    let message = lines[0]["message"].as_str().unwrap();
    assert!(message.contains("already running"), "got: {message}");
    assert!(
        message.contains("active"),
        "the original status is included: {message}"
    );

    assert!(
        recorder.requests("turn/start").is_empty(),
        "the prompt must not be folded into the running turn"
    );
}

#[test]
fn run_refuses_a_thread_and_a_cwd_together() {
    // clap decides this one: no connection is ever made.
    let output = run(
        dead_port(),
        &["run", "--thread", "T1", "--cwd", "/tmp", "--prompt", "hi"],
    );
    assert_eq!(code(&output), 4, "a usage error is 4, not clap's own 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot be used with"), "got: {stderr}");
    assert!(stderr.contains("--cwd"), "got: {stderr}");
}

#[test]
fn help_and_version_are_not_usage_errors() {
    let port = dead_port();
    for flag in ["--help", "--version"] {
        let output = run(port, &[flag]);
        assert_eq!(code(&output), 0, "{flag} prints and exits clean");
        assert!(
            !output.stdout.is_empty(),
            "{flag} prints on stdout, not stderr"
        );
    }
    // A missing required argument is the other half of the same decision.
    let output = run(port, &["run", "--cwd", "/tmp"]);
    assert_eq!(code(&output), 4, "a missing --prompt is a usage error");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--prompt"),
        "clap's own message still goes to stderr"
    );
}
