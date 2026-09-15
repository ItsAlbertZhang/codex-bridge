//! Integration tests against an in-process mock bridge-plugin.
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

use common::dsh::{
    active, base_handler, cli, code, idle, lines, notify, ok, server_request, start_mock,
    thread_value, Handler, Recorder, TempDir, Tx,
};

const CWD: &str = env!("CARGO_MANIFEST_DIR");

fn run(port: u16, args: &[&str]) -> Output {
    let state = TempDir::new("session");
    cli(port, &state)
        .args(args)
        .output()
        .expect("running agent-bridge")
}

#[test]
fn handshake_precedes_every_request() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({ "turnId": "U1" }));
            }
        },
    ));

    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi", "--no-wait"]);
    assert_eq!(code(&output), 0);

    let seen = recorder.all();
    assert_eq!(seen[0]["method"], "initialize");
    assert_eq!(seen[0]["params"]["clientInfo"]["name"], "agent-bridge");
    assert!(seen[0]["id"].is_string(), "all protocol ids are strings");
    assert_eq!(seen[0]["params"]["clientInfo"]["version"], "0.2.0");
    assert_eq!(recorder.requests("initialize").len(), 1);
    assert!(recorder.requests("initialized").is_empty());
    assert_eq!(seen[1]["method"], "thread/start");
    assert!(seen.iter().all(|message| message["id"].is_string()));

    let started = &lines(&output)[0];
    assert_eq!(started["event"], "started");
    assert_eq!(started["threadId"], "T1");
    assert_eq!(started["turnId"], "U1");
    assert_eq!(started["title"], "dsh-T1");
    assert_eq!(started["uiUrl"], "http://127.0.0.1:18080/");
}

#[test]
fn the_started_event_prefers_the_logged_token_url_over_the_handshake_one() {
    // `initialize` can only report the token-free URL; the token the human
    // needs is in the daemon log, so the last complete line there wins.
    let recorder = Recorder::default();
    let port = start_mock(base_handler(recorder, "T1", idle(), |msg, tx, method| {
        if method == "turn/start" {
            ok(tx, msg, json!({ "turnId": "U1" }));
        }
    }));
    let state = TempDir::new("started-token-url");
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:12899/?token=OLD\r\n\
         unrelated line\r\n\
         dsh web: http://127.0.0.1:12899/?token=CURRENT\r\n",
    )
    .expect("daemon log");

    let output = cli(port, &state)
        .args(["run", "--cwd", CWD, "--prompt", "hi", "--no-wait"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    let started = &lines(&output)[0];
    assert_eq!(started["event"], "started");
    assert_eq!(started["uiUrl"], "http://127.0.0.1:12899/?token=CURRENT");
}

#[test]
fn the_started_event_falls_back_to_the_handshake_url_without_a_logged_one() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(recorder, "T1", idle(), |msg, tx, method| {
        if method == "turn/start" {
            ok(tx, msg, json!({ "turnId": "U1" }));
        }
    }));
    let state = TempDir::new("started-handshake-url");
    std::fs::write(state.join("dsh/daemon.log"), "dsh starting\n").expect("daemon log");

    let output = cli(port, &state)
        .args(["run", "--cwd", CWD, "--prompt", "hi", "--no-wait"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    assert_eq!(lines(&output)[0]["uiUrl"], "http://127.0.0.1:18080/");
}

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
            ok(tx, msg, json!({ "turnId": "U1" }));
            notify(
                tx,
                "agent/assistant-stream",
                json!({ "threadId": "T1", "delta": "ignored" }),
            );
            notify(
                tx,
                "thread/event",
                json!({
                    "threadId": "T1",
                    "seq": 1, "type": "assistant/message", "data": { "content": [{"type": "text", "text": "all done"}] }
                }),
            );
            notify(
                tx,
                "turn/completed",
                json!({
                    "threadId": "T1",
                    "turnId": "U1", "status": "completed", "reason": "completed",
                    "finalMessage": "all done"
                }),
            );
        },
    ));

    let output = run(
        port,
        &["run", "--cwd", CWD, "--prompt", "hi", "--effort", "low"],
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
    assert_eq!(turn["reason"], "completed");

    let turn_start = &recorder.requests("turn/start")[0];
    assert_eq!(
        turn_start["params"],
        json!({"threadId": "T1", "text": "hi"})
    );
    assert_eq!(
        recorder.requests("thread/start")[0]["params"]["reasoningEffort"],
        "low"
    );
}

#[test]
fn another_threads_turn_does_not_end_the_wait_or_trigger_unsubscribe() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method != "turn/start" {
                return;
            }
            ok(tx, msg, json!({ "turnId": "U1" }));
            // A connection can subscribe to multiple ordinary threads.
            server_request(
                tx,
                "req-other",
                "approval/request",
                json!({
                    "threadId": "OTHER", "requestId": "req-other", "toolName": "shell"
                }),
            );
            server_request(
                tx,
                "req-unscoped",
                "approval/request",
                json!({
                    "requestId": "req-unscoped", "toolName": "shell"
                }),
            );
            notify(
                tx,
                "turn/started",
                json!({ "threadId": "OTHER", "turnId": "OU1" }),
            );
            notify(
                tx,
                "turn/completed",
                json!({ "threadId": "OTHER", "turnId": "OU1", "status": "failed", "reason": "error" }),
            );
            notify(
                tx,
                "thread/event",
                json!({
                    "threadId": "T1",
                    "seq": 1, "type": "assistant/message", "data": { "content": [{"type": "text", "text": "main answer"}] }
                }),
            );
            notify(
                tx,
                "turn/completed",
                json!({ "threadId": "T1", "turnId": "U1", "status": "completed" }),
            );
        },
    ));

    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi"]);
    assert_eq!(
        code(&output),
        0,
        "another thread's failure must not leak out"
    );

    let lines = lines(&output);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["status"], "completed");
    assert_eq!(lines[1]["finalMessage"], "main answer");

    assert!(recorder.requests("thread/unsubscribe").is_empty());
    assert!(
        recorder.all().iter().all(|msg| msg.get("method").is_some()),
        "requests for another or missing threadId must stay unanswered"
    );
}

#[test]
fn a_server_request_exits_two_with_verbatim_params() {
    let recorder = Recorder::default();
    let params = json!({
        "threadId": "T1",
        "requestId": "req-55",
        "toolName": "shell",
        "callId": "call-9",
        "reason": "write outside workspace"
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
            ok(tx, msg, json!({ "turnId": "U1" }));
            server_request(tx, "req-55", "approval/request", params.clone());
        },
    ));

    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi"]);
    assert_eq!(code(&output), 2);

    let lines = lines(&output);
    let request = &lines[1];
    assert_eq!(request["event"], "request");
    assert_eq!(request["threadId"], "T1");
    assert_eq!(request["requestId"], "req-55");
    assert_eq!(request["method"], "approval/request");
    assert_eq!(request["params"], expected);

    // The request must stay pending on the server.
    assert!(
        !recorder
            .all()
            .iter()
            .any(|m| m.get("id") == Some(&json!("req-55"))),
        "agent-bridge must not answer the request it handed over"
    );
}

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
                    ok(tx, msg, thread_value("T1", active(), json!([])));
                    // The plugin re-sends the pending request with the same id.
                    server_request(
                        tx,
                        "req-77",
                        "approval/request",
                        json!({ "threadId": "T1", "requestId": "req-77", "toolName": "shell" }),
                    );
                }
                _ => {
                    // The decision arrives as a JSON-RPC response, not a request.
                    if msg.get("id") == Some(&json!("req-77")) {
                        notify(
                            tx,
                            "thread/event",
                            json!({
                                "threadId": "T1",
                                "seq": 1, "type": "assistant/message", "data": { "content": [{"type": "text", "text": "approved and done"}] }
                            }),
                        );
                        notify(
                            tx,
                            "turn/completed",
                            json!({ "threadId": "T1", "turnId": "U1", "status": "completed" }),
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
            "req-77",
            "--decision",
            "accept",
        ],
    );
    assert_eq!(code(&output), 0);

    let answer = recorder
        .all()
        .into_iter()
        .find(|m| m.get("id") == Some(&json!("req-77")))
        .expect("the decision must reach the server");
    assert_eq!(answer["result"], json!({ "decision": "allowed-once" }));

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "turn");
    assert_eq!(lines[0]["finalMessage"], "approved and done");
}

#[test]
fn reply_without_an_id_waits_for_the_full_window_and_deduplicates_requests() {
    let recorder = Recorder::default();
    let (reply_seen, replied) = std::sync::mpsc::channel();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        move |msg, tx, method| {
            if method == "thread/resume" {
                ok(tx, msg, thread_value("T1", active(), json!([])));
                server_request(
                    tx,
                    "req-77",
                    "approval/request",
                    json!({"threadId": "T1", "requestId": "req-77", "toolName": "shell"}),
                );
                let tx = tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(2));
                    server_request(
                        &tx,
                        "req-77",
                        "approval/request",
                        json!({"threadId": "T1", "requestId": "req-77", "toolName": "shell"}),
                    );
                    // Completion arrives during collection and must be replayed
                    // only after the complete window selects and answers req-77.
                    notify(
                        &tx,
                        "turn/completed",
                        json!({
                            "threadId": "T1", "turnId": 7, "status": "completed",
                            "reason": "completed", "durationMs": 2345,
                            "finalMessage": "queued before replying"
                        }),
                    );
                });
            } else if method.is_empty() && msg["id"] == "req-77" {
                let _ = reply_seen.send(msg.clone());
            }
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
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(started.elapsed() >= Duration::from_secs(10));
    let response = replied
        .recv_timeout(Duration::from_secs(1))
        .expect("the mock observes the response before its recorder is inspected");
    assert_eq!(response["id"], "req-77");
    let answers: Vec<_> = recorder
        .all()
        .into_iter()
        .filter(|msg| msg.get("method").is_none() && msg.get("id").is_some())
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "same string request ID must be answered once"
    );
    assert_eq!(answers[0]["id"], "req-77");
    assert_eq!(answers[0]["result"], json!({"decision": "allowed-once"}));
    let events = lines(&output);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "turn");
    assert_eq!(events[0]["turnId"], "7");
    assert_eq!(events[0]["status"], "completed");
    assert_eq!(events[0]["reason"], "completed");
    assert_eq!(events[0]["durationMs"], 2345);
    assert_eq!(events[0]["finalMessage"], "queued before replying");
}

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
                thread_value(
                    "T1",
                    active(),
                    json!([
                        { "turnId": "U1", "status": "running" },
                        { "turnId": "U2", "status": "running" },
                        { "turnId": "U3", "status": "completed" },
                    ]),
                ),
            ),
            "turn/steer" => ok(tx, msg, json!({"turnId": msg["params"]["expectedTurnId"]})),
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
    assert_eq!(requests[0]["params"]["text"], "change course");
    assert_eq!(lines(&output)[0]["event"], "steered");
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
                ok(tx, msg, json!({ "turnId": "U1" }));
            }
        },
    ));

    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi"]);
    assert_eq!(code(&output), 4);

    let lines = lines(&output);
    assert_eq!(lines[0]["event"], "error");
    let message = lines[0]["message"].as_str().unwrap();
    assert!(message.contains("already running"), "got: {message}");

    assert!(
        recorder.requests("turn/start").is_empty(),
        "the prompt must not be folded into the running turn"
    );
}

#[test]
fn wait_on_an_idle_thread_reports_the_last_turn() {
    let turns = json!([
        { "turnId": "U1", "status": "completed", "finalMessage": "older" },
        { "turnId": "U2", "status": "failed", "finalMessage": "newest" }
    ]);
    let recorder = Recorder::default();
    let inner = recorder.clone();
    let handler: Handler = Arc::new(move |msg: &Value, tx: &Tx| {
        inner.push(msg.clone());
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/resume" => ok(tx, msg, thread_value("T1", idle(), json!([]))),
            "thread/read" => ok(tx, msg, thread_value("T1", idle(), turns.clone())),
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
                ok(tx, msg, json!({ "turnId": "U1" }));
                server_request(
                    tx,
                    "req-9",
                    "approval/request",
                    json!({ "threadId": "T1", "requestId": "req-9", "toolName": "shell" }),
                );
            }
            _ => {
                if msg.get("id") == Some(&json!("req-9")) {
                    notify(
                        tx,
                        "turn/completed",
                        json!({ "threadId": "T1", "turnId": "U1", "status": "completed" }),
                    );
                }
            }
        },
    ));

    let output = run(
        port,
        &["run", "--cwd", CWD, "--prompt", "hi", "--auto-decline"],
    );
    assert_eq!(code(&output), 0);
    let lines = lines(&output);
    assert_eq!(lines.len(), 2, "the declined request is not a stdout event");
    assert_eq!(lines[1]["event"], "turn");

    let answer = recorder
        .all()
        .into_iter()
        .find(|m| m.get("id") == Some(&json!("req-9")))
        .expect("the decline must reach the server");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("agent-bridge is running this turn unattended:"),
        "got: {message}"
    );
}

#[test]
fn status_and_read_print_their_own_shapes() {
    let turns = json!([
        { "turnId": "U1", "status": "completed", "finalMessage": "hello" }
    ]);
    let handler: Handler = Arc::new(move |msg: &Value, tx: &Tx| {
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/read" => {
                let include = msg["params"]["includeTurns"].as_bool().unwrap_or(false);
                let turns = if include { turns.clone() } else { json!([]) };
                ok(tx, msg, thread_value("T1", idle(), turns));
            }
            _ => {}
        }
    });
    let port = start_mock(handler);

    let status = run(port, &["status", "--thread", "T1"]);
    assert_eq!(code(&status), 0);
    let status = &lines(&status)[0];
    assert_eq!(status["threadId"], "T1");
    assert_eq!(status["status"], "idle");
    assert_eq!(status["cwd"], CWD);
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
fn run_on_an_existing_idle_thread_starts_a_turn_without_opening_one() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| match method {
            "thread/resume" => ok(tx, msg, thread_value("T1", idle(), json!([]))),
            "turn/start" => {
                ok(tx, msg, json!({ "turnId": "U2" }));
                notify(
                    tx,
                    "thread/event",
                    json!({
                        "threadId": "T1",
                        "seq": 1, "type": "assistant/message", "data": { "content": [{"type": "text", "text": "second answer"}] }
                    }),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({ "threadId": "T1", "turnId": "U2", "status": "completed" }),
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
    assert_eq!(turn_start["params"]["text"], "again");

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
            "thread/resume" => ok(tx, msg, thread_value("T1", active(), json!([]))),
            "turn/start" => ok(tx, msg, json!({ "turnId": "U2" })),
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
        recorder.requests("turn/start").is_empty(),
        "the prompt must not be folded into the running turn"
    );
}

#[test]
fn run_passes_creation_options_and_file_contents_without_rewriting_them() {
    let state = TempDir::new("session-options");
    let prompt_path = state.join("prompt.txt");
    let instructions_path = state.join("instructions.txt");
    std::fs::write(&prompt_path, "line one\nline two\n").unwrap();
    std::fs::write(&instructions_path, "Keep the API stable.\n").unwrap();
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => ok(tx, msg, json!({"uiUrl": "http://127.0.0.1:18080/"})),
            "thread/start" => ok(
                tx,
                msg,
                json!({
                    "threadId": "T1", "title": msg["params"]["title"], "cwd": msg["params"]["cwd"]
                }),
            ),
            "thread/read" => ok(tx, msg, thread_value("T1", idle(), json!([]))),
            "turn/start" => ok(tx, msg, json!({"turnId": "U1"})),
            _ => {}
        }
    }));
    let output = cli(port, &state)
        .args(["run", "--cwd", CWD, "--prompt-file"])
        .arg(&prompt_path)
        .args(["--developer-instructions-file"])
        .arg(&instructions_path)
        .args([
            "--sandbox",
            "workspace-write",
            "--approval",
            "ask",
            "--model",
            "mock-model",
            "--effort",
            "high",
            "--title",
            "Review API changes",
            "--no-wait",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(
        recorder.requests("thread/start")[0]["params"],
        json!({
            "cwd": CWD, "sandbox": "workspace-write", "approval": "ask", "model": "mock-model",
            "reasoningEffort": "high", "title": "Review API changes",
            "developerInstructions": "Keep the API stable.\n"
        })
    );
    assert_eq!(
        recorder.requests("turn/start")[0]["params"],
        json!({
            "threadId": "T1", "text": "line one\nline two\n"
        })
    );
    assert_eq!(lines(&output)[0]["title"], "Review API changes");
    assert_eq!(lines(&output)[0]["uiUrl"], "http://127.0.0.1:18080/");
}

#[test]
fn creation_options_conflict_with_an_existing_thread_before_connecting() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(recorder.clone(), "T1", idle(), |_, _, _| {}));
    for (flag, value) in [
        ("--title", "new title"),
        ("--sandbox", "read-only"),
        ("--approval", "never"),
        ("--model", "model"),
        ("--effort", "low"),
        ("--developer-instructions-file", "instructions.txt"),
    ] {
        let output = run(
            port,
            &["run", "--thread", "T1", "--prompt", "hello", flag, value],
        );
        assert_eq!(code(&output), 4, "{flag}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    }
    assert!(recorder.all().is_empty());
}

#[test]
fn unsupported_decisions_and_options_are_usage_errors_before_connecting() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(recorder.clone(), "T1", idle(), |_, _, _| {}));
    for args in [
        vec![
            "reply",
            "--thread",
            "T1",
            "--decision",
            "accept-for-session",
        ],
        vec![
            "run",
            "--cwd",
            CWD,
            "--prompt",
            "hello",
            "--approval",
            "on-request",
        ],
        vec![
            "run",
            "--cwd",
            CWD,
            "--prompt",
            "hello",
            "--approval",
            "untrusted",
        ],
        vec!["run", "--cwd", CWD, "--prompt", "hello", "--no-pane"],
    ] {
        let output = run(port, &args);
        assert_eq!(code(&output), 4, "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert!(recorder.all().is_empty());
}

fn pending_server(method: &'static str, params: Value, recorder: Recorder) -> u16 {
    start_mock(base_handler(
        recorder,
        "T1",
        active(),
        move |msg, tx, received| {
            if received == "thread/resume" {
                let mut thread = thread_value("T1", active(), json!([]));
                thread["pendingRequests"] = json!(["req-100"]);
                ok(tx, msg, thread);
                server_request(tx, "req-100", method, params.clone());
            } else if msg["id"] == "req-100" && msg.get("method").is_none() {
                notify(
                    tx,
                    "turn/completed",
                    json!({
                        "threadId": "T1", "turnId": "U1", "status": "completed", "reason": "completed",
                        "finalMessage": "finished after reply"
                    }),
                );
            }
        },
    ))
}

#[test]
fn reply_decline_maps_to_rejected() {
    let recorder = Recorder::default();
    let port = pending_server(
        "approval/request",
        json!({
            "threadId": "T1", "requestId": "req-100", "toolName": "shell", "reason": "permission"
        }),
        recorder.clone(),
    );
    let output = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "req-100",
            "--decision",
            "decline",
        ],
    );
    assert_eq!(code(&output), 0, "{output:?}");
    let answer = recorder
        .all()
        .into_iter()
        .find(|msg| msg["id"] == "req-100")
        .unwrap();
    assert_eq!(answer["result"], json!({"decision": "rejected"}));
    assert!(answer.get("method").is_none());
    assert_eq!(lines(&output)[0]["finalMessage"], "finished after reply");
}

#[test]
fn user_question_requests_keep_verbatim_params_and_use_result_json_answers() {
    let params = json!({
        "threadId": "T1", "requestId": "req-100", "questions": [{
            "id": "q1", "question": "Which checks?", "detail": "Choose all that apply.", "header": "Checks",
            "options": [{"label": "unit", "description": "Fast checks"}, {"label": "integration"}], "multiSelect": true
        }]
    });
    let recorder = Recorder::default();
    let port = pending_server("userQuestion/request", params.clone(), recorder.clone());
    let pending = run(port, &["wait", "--thread", "T1"]);
    assert_eq!(code(&pending), 2);
    assert_eq!(lines(&pending)[0]["method"], "userQuestion/request");
    assert_eq!(lines(&pending)[0]["params"], params);
    let answer = json!({"answers": [{"id": "q1", "selected": ["unit", "integration"], "custom": "Also inspect logs"}]});
    let output = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "req-100",
            "--result-json",
            &answer.to_string(),
        ],
    );
    assert_eq!(code(&output), 0, "{output:?}");
    let responses: Vec<_> = recorder
        .all()
        .into_iter()
        .filter(|msg| msg["id"] == "req-100")
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["result"], answer);
}

#[test]
fn decisions_require_approval_requests_while_result_json_is_sent_verbatim() {
    for (method, args, exit_code) in [
        ("userQuestion/request", ["--decision", "accept"], 4),
        (
            "approval/request",
            ["--result-json", "{\"decision\":\"rejected\"}"],
            0,
        ),
    ] {
        let recorder = Recorder::default();
        let port = pending_server(
            method,
            json!({"threadId": "T1", "requestId": "req-100", "toolName": "shell", "questions": []}),
            recorder.clone(),
        );
        let output = run(
            port,
            &[
                "reply",
                "--thread",
                "T1",
                "--request-id",
                "req-100",
                args[0],
                args[1],
            ],
        );
        assert_eq!(code(&output), exit_code, "{method}: {output:?}");
        if exit_code == 4 {
            assert_eq!(lines(&output)[0]["event"], "error");
            assert!(recorder.all().iter().all(|msg| msg.get("method").is_some()));
        } else {
            let answer = recorder
                .all()
                .into_iter()
                .find(|msg| msg["id"] == "req-100")
                .unwrap();
            assert_eq!(answer["result"], json!({"decision": "rejected"}));
        }
    }
}

#[test]
fn a_resolved_explicit_request_exits_four_without_answering() {
    for resolved_by in ["client", "browser", "cancelled"] {
        let recorder = Recorder::default();
        let port = start_mock(base_handler(
            recorder.clone(),
            "T1",
            active(),
            move |msg, tx, method| {
                if method == "thread/resume" {
                    ok(tx, msg, thread_value("T1", active(), json!([])));
                    notify(
                        tx,
                        "request/resolved",
                        json!({"threadId": "T1", "requestId": "req-100", "resolvedBy": resolved_by}),
                    );
                    notify(
                        tx,
                        "turn/completed",
                        json!({"threadId": "T1", "turnId": "U1", "status": "interrupted", "reason": "aborted"}),
                    );
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
                "req-100",
                "--decision",
                "accept",
            ],
        );
        assert_eq!(code(&output), 4, "{resolved_by}: {output:?}");
        assert_eq!(lines(&output)[0]["event"], "error");
        assert!(recorder.all().iter().all(|msg| msg.get("method").is_some()));
    }
}

#[test]
fn implicit_reply_keeps_full_window_ambiguity_even_when_one_request_resolves() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| {
            if method == "thread/resume" {
                ok(tx, msg, thread_value("T1", active(), json!([])));
                server_request(
                    tx,
                    "req-100",
                    "approval/request",
                    json!({"threadId": "T1", "requestId": "req-100", "toolName": "shell"}),
                );
                notify(
                    tx,
                    "request/resolved",
                    json!({"threadId": "T1", "requestId": "req-100", "resolvedBy": "browser"}),
                );
                server_request(
                    tx,
                    "req-101",
                    "approval/request",
                    json!({"threadId": "T1", "requestId": "req-101", "toolName": "shell"}),
                );
            } else if msg["id"] == "req-101" {
                notify(
                    tx,
                    "turn/completed",
                    json!({"threadId": "T1", "turnId": "U1", "status": "completed", "reason": "completed"}),
                );
            }
        },
    ));
    let output = run(port, &["reply", "--thread", "T1", "--decision", "decline"]);
    assert_eq!(code(&output), 4, "{output:?}");
    let responses: Vec<_> = recorder
        .all()
        .into_iter()
        .filter(|msg| msg.get("method").is_none())
        .collect();
    assert!(responses.is_empty());
    assert!(lines(&output)[0]["message"]
        .as_str()
        .unwrap()
        .contains("--request-id"));
}

#[test]
fn reply_during_browser_cancellation_is_an_error_but_wait_reports_interrupted() {
    let port = start_mock(Arc::new(|msg, tx| {
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/resume" => {
                ok(tx, msg, thread_value("T1", active(), json!([])));
                notify(
                    tx,
                    "request/resolved",
                    json!({"threadId": "T1", "requestId": "req-100", "resolvedBy": "cancelled"}),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({"threadId": "T1", "turnId": "U1", "status": "interrupted", "reason": "aborted", "finalMessage": ""}),
                );
            }
            _ => {}
        }
    }));
    let reply = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "req-100",
            "--decision",
            "decline",
        ],
    );
    assert_eq!(code(&reply), 4, "{reply:?}");
    assert_eq!(lines(&reply)[0]["event"], "error");
    let wait = run(port, &["wait", "--thread", "T1"]);
    assert_eq!(code(&wait), 1, "{wait:?}");
    assert_eq!(lines(&wait)[0]["status"], "interrupted");
}

#[test]
fn explicit_reply_on_an_already_idle_thread_keeps_the_last_turn_shortcut() {
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/resume" => ok(tx, msg, thread_value("T1", idle(), json!([]))),
            "thread/read" => ok(
                tx,
                msg,
                thread_value(
                    "T1",
                    idle(),
                    json!([
                        {"turnId": "U1", "status": "interrupted", "reason": "aborted", "finalMessage": "partial answer"}
                    ]),
                ),
            ),
            _ => {}
        }
    }));
    let output = run(
        port,
        &[
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "req-100",
            "--decision",
            "decline",
        ],
    );
    assert_eq!(code(&output), 1, "{output:?}");
    assert_eq!(lines(&output)[0]["event"], "turn");
    assert_eq!(lines(&output)[0]["status"], "interrupted");
    assert_eq!(lines(&output)[0]["finalMessage"], "partial answer");
    assert!(recorder.all().iter().all(|msg| msg.get("method").is_some()));
}

#[test]
fn completed_final_message_overrides_event_text_even_when_empty() {
    for final_message in ["authoritative answer", ""] {
        let port = start_mock(base_handler(
            Recorder::default(),
            "T1",
            idle(),
            move |msg, tx, method| {
                if method == "turn/start" {
                    ok(tx, msg, json!({"turnId": "U1"}));
                    notify(
                        tx,
                        "thread/event",
                        json!({"threadId": "T1", "seq": 1, "type": "assistant/message", "data": {"content": [{"type": "text", "text": "earlier answer"}]}}),
                    );
                    notify(
                        tx,
                        "turn/completed",
                        json!({"threadId": "T1", "turnId": "U1", "status": "completed", "reason": "completed", "finalMessage": final_message}),
                    );
                }
            },
        ));
        let output = run(port, &["run", "--cwd", CWD, "--prompt", "hello"]);
        assert_eq!(code(&output), 0, "{output:?}");
        assert_eq!(lines(&output)[1]["finalMessage"], final_message);
    }
}

#[test]
fn completion_preserves_reason_and_omits_absent_or_null_optional_fields() {
    for reason in [None, Some(Value::Null), Some(json!("max-tokens"))] {
        let expected = reason.clone().filter(|value| !value.is_null());
        let port = start_mock(base_handler(
            Recorder::default(),
            "T1",
            idle(),
            move |msg, tx, method| {
                if method == "turn/start" {
                    ok(tx, msg, json!({"turnId": "U1"}));
                    let mut completion = json!({
                        "threadId": "T1", "turnId": "U1", "status": "failed",
                        "error": null, "durationMs": null, "finalMessage": "partial"
                    });
                    if let Some(reason) = &reason {
                        completion["reason"] = reason.clone();
                    }
                    notify(tx, "turn/completed", completion);
                }
            },
        ));
        let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi"]);
        assert_eq!(code(&output), 1, "{output:?}");
        let events = lines(&output);
        let turn = &events[1];
        assert_eq!(turn["status"], "failed");
        assert_eq!(turn.get("reason"), expected.as_ref());
        assert!(turn.get("error").is_none());
        assert!(turn.get("durationMs").is_none());
    }
}

#[test]
fn event_fallback_joins_text_blocks_and_ignores_empty_messages() {
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
                notify(
                    tx,
                    "thread/event",
                    json!({"threadId": "T1", "seq": 1, "type": "assistant/message", "data": {"content": [
                        {"type": "text", "text": "first "}, {"type": "image", "url": "unused"}, {"type": "text", "text": "second"}
                    ]}}),
                );
                notify(
                    tx,
                    "thread/event",
                    json!({"threadId": "T1", "seq": 2, "type": "assistant/message", "data": {"content": []}}),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({"threadId": "T1", "turnId": "U1", "status": "completed", "reason": "completed"}),
                );
            }
        },
    ));
    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hello"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(lines(&output)[1]["finalMessage"], "first second");
}

#[test]
fn event_fallback_reads_the_nested_message_sent_by_the_live_plugin() {
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": 7}));
                notify(
                    tx,
                    "thread/event",
                    json!({
                        "threadId": "T1", "seq": 1, "type": "assistant/message",
                        "data": {"message": {"content": [
                            {"type": "reasoning", "text": "private reasoning"},
                            {"type": "text", "text": "nested "},
                            {"type": "tool-call", "name": "unused"},
                            {"type": "text", "text": "answer"}
                        ]}}
                    }),
                );
                notify(
                    tx,
                    "turn/completed",
                    json!({
                        "threadId": "T1", "turnId": 7, "status": "completed", "reason": "completed"
                    }),
                );
            }
        },
    ));
    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(lines(&output)[1]["turnId"], "7");
    assert_eq!(lines(&output)[1]["finalMessage"], "nested answer");
}

#[test]
fn dsh_never_opens_a_herdr_pane() {
    let state = TempDir::new("dsh-no-pane");
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
            }
        },
    ));
    let output = cli(port, &state)
        .env("HERDR_ENV", "1")
        .env("AGENT_BRIDGE_CODEX_HERDR_BIN", common::fake_herdr(&state))
        .env("FAKE_HERDR_LOG", state.join("herdr-args.txt"))
        .env("FAKE_HERDR_AGENT_GET", "1")
        .args(["run", "--cwd", CWD, "--prompt", "hi", "--no-wait"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(!state.join("herdr-args.txt").exists());
}

#[test]
fn read_and_wait_do_not_reuse_an_older_turns_answer() {
    let port = start_mock(Arc::new(|msg, tx| {
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/resume" => ok(tx, msg, thread_value("T1", idle(), json!([]))),
            "thread/read" => ok(
                tx,
                msg,
                thread_value(
                    "T1",
                    idle(),
                    json!([
                        {"turnId": "U1", "status": "completed", "finalMessage": "stale answer"},
                        {"turnId": "U2", "status": "completed", "finalMessage": ""}
                    ]),
                ),
            ),
            _ => {}
        }
    }));
    for command in ["read", "wait"] {
        let output = run(port, &[command, "--thread", "T1"]);
        assert_eq!(code(&output), 0, "{output:?}");
        assert_eq!(lines(&output)[0]["finalMessage"], "");
    }
}

#[test]
fn thread_events_reset_stall_for_every_event_type() {
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for (seq, kind) in ["step/start", "tool/call", "tool/result", "session/title"]
                        .iter()
                        .enumerate()
                    {
                        std::thread::sleep(Duration::from_millis(400));
                        notify(
                            &tx,
                            "thread/event",
                            json!({"threadId": "T1", "seq": seq, "type": kind, "data": {}}),
                        );
                    }
                    notify(
                        &tx,
                        "turn/completed",
                        json!({"threadId": "T1", "turnId": "U1", "status": "completed", "reason": "completed", "finalMessage": "done"}),
                    );
                });
            }
        },
    ));
    let output = run(
        port,
        &[
            "run",
            "--cwd",
            CWD,
            "--prompt",
            "hello",
            "--stall-secs",
            "1",
            "--timeout-secs",
            "5",
        ],
    );
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(lines(&output)[1]["event"], "turn");
}

#[test]
fn status_started_and_stream_notifications_do_not_reset_stall() {
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for _ in 0..15 {
                        std::thread::sleep(Duration::from_millis(200));
                        notify(
                            &tx,
                            "thread/status",
                            json!({"threadId": "T1", "status": "running"}),
                        );
                        notify(
                            &tx,
                            "turn/started",
                            json!({"threadId": "T1", "turnId": "ignored-late-start"}),
                        );
                        notify(
                            &tx,
                            "agent/assistant-stream",
                            json!({"threadId": "T1", "delta": "still thinking"}),
                        );
                    }
                });
            }
        },
    ));
    let output = run(
        port,
        &[
            "run",
            "--cwd",
            CWD,
            "--prompt",
            "hello",
            "--stall-secs",
            "1",
            "--timeout-secs",
            "3",
        ],
    );
    assert_eq!(code(&output), 3, "{output:?}");
    assert_eq!(lines(&output)[1]["event"], "stalled");
    assert_eq!(
        lines(&output)[1]["turnId"],
        "U1",
        "a known turn ID is not overwritten"
    );
}

#[test]
fn follow_keeps_stalling_but_timeout_is_a_hard_bound() {
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
            }
        },
    ));
    let output = run(
        port,
        &[
            "run",
            "--cwd",
            CWD,
            "--prompt",
            "hello",
            "--stall-secs",
            "1",
            "--timeout-secs",
            "2",
            "--follow",
        ],
    );
    assert_eq!(code(&output), 3, "{output:?}");
    let events = lines(&output);
    assert!(events.iter().any(|event| event["event"] == "stalled"));
    assert_eq!(events.last().unwrap()["event"], "timeout");
    assert_eq!(events.last().unwrap()["timeoutSecs"], 2);
}

#[test]
fn interrupt_reads_the_latest_running_turn_and_sends_flat_params() {
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/read" => ok(
                tx,
                msg,
                thread_value(
                    "T1",
                    active(),
                    json!([
                        {"turnId": "U1", "status": "completed"}, {"turnId": "U2", "status": "running"}
                    ]),
                ),
            ),
            "turn/interrupt" => ok(tx, msg, json!({})),
            _ => {}
        }
    }));
    let output = run(port, &["interrupt", "--thread", "T1"]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(
        recorder.requests("thread/read")[0]["params"],
        json!({"threadId": "T1", "includeTurns": true})
    );
    assert_eq!(
        recorder.requests("turn/interrupt")[0]["params"],
        json!({"threadId": "T1", "turnId": "U2"})
    );
    assert_eq!(lines(&output)[0]["event"], "interrupted");
    assert_eq!(lines(&output)[0]["turnId"], "U2");
}

#[test]
fn interrupt_refuses_an_idle_thread_without_sending_a_cancel() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(recorder.clone(), "T1", idle(), |_, _, _| {}));
    let output = run(port, &["interrupt", "--thread", "T1"]);
    assert_eq!(code(&output), 4, "{output:?}");
    assert!(recorder.requests("turn/interrupt").is_empty());
    assert_eq!(lines(&output)[0]["event"], "error");
}

#[test]
fn json_rpc_error_kinds_have_actionable_messages_and_exit_four() {
    for (kind, expected) in [
        ("thread_not_found", "not found"),
        ("thread_busy", "running"),
        ("no_active_turn", "active"),
        ("turn_mismatch", "turn"),
        ("invalid_cwd", "directory"),
        ("not_resumable", "resum"),
        ("internal", "internal"),
    ] {
        let port = start_mock(Arc::new(move |msg, tx| {
            match msg["method"].as_str().unwrap_or("") {
                "initialize" => ok(tx, msg, json!({})),
                "thread/read" => {
                    let _ = tx.send(json!({"jsonrpc": "2.0", "id": msg["id"], "error": {
                        "code": -32000, "message": "backend details", "data": {"kind": kind}
                    }}));
                }
                _ => {}
            }
        }));
        let output = run(port, &["status", "--thread", "T1"]);
        assert_eq!(code(&output), 4, "{kind}: {output:?}");
        let events = lines(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "error");
        let message = events[0]["message"].as_str().unwrap();
        assert!(message.contains(expected), "{kind}: {message}");
        assert!(message.contains("backend details"), "{kind}: {message}");
    }
}

#[test]
fn turn_start_still_handles_a_server_busy_race_after_an_idle_read() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": msg["id"], "error": {
                "code": -32000, "message": "a browser turn just started", "data": {"kind": "thread_busy"}
            }}));
            }
        },
    ));
    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hello"]);
    assert_eq!(code(&output), 4, "{output:?}");
    assert_eq!(recorder.requests("thread/read").len(), 1);
    assert_eq!(recorder.requests("turn/start").len(), 1);
    assert_eq!(lines(&output)[0]["event"], "error");
    assert!(lines(&output)
        .iter()
        .all(|event| event["event"] != "started"));
}

#[test]
fn omitted_options_stay_omitted_and_sandbox_values_are_passed_through() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
            }
        },
    ));
    let plain = run(
        port,
        &["run", "--cwd", CWD, "--prompt", "hello", "--no-wait"],
    );
    assert_eq!(code(&plain), 0, "{plain:?}");
    assert_eq!(
        recorder.requests("thread/start")[0]["params"],
        json!({"cwd": CWD})
    );
    for sandbox in ["read-only", "workspace-write", "danger-full-access"] {
        let output = run(
            port,
            &[
                "run",
                "--cwd",
                CWD,
                "--prompt",
                "hello",
                "--sandbox",
                sandbox,
                "--approval",
                "never",
                "--no-wait",
            ],
        );
        assert_eq!(code(&output), 0, "{sandbox}: {output:?}");
        let starts = recorder.requests("thread/start");
        assert_eq!(
            starts.last().unwrap()["params"],
            json!({"cwd": CWD, "sandbox": sandbox, "approval": "never"})
        );
    }
}

#[test]
fn explicit_reply_waits_for_its_request_without_answering_earlier_requests() {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        active(),
        |msg, tx, method| {
            if method == "thread/resume" {
                let mut thread = thread_value("T1", active(), json!([]));
                thread["pendingRequests"] = json!(["req-100", "req-101"]);
                ok(tx, msg, thread);
                for request_id in ["req-100", "req-101"] {
                    server_request(
                        tx,
                        request_id,
                        "approval/request",
                        json!({"threadId": "T1", "requestId": request_id, "toolName": "shell"}),
                    );
                }
            } else if msg["id"] == "req-101" && msg.get("method").is_none() {
                notify(
                    tx,
                    "turn/completed",
                    json!({"threadId": "T1", "turnId": "U1", "status": "completed", "reason": "completed"}),
                );
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
            "req-101",
            "--decision",
            "accept",
            "--auto-decline",
        ],
    );
    assert_eq!(code(&output), 0, "{output:?}");
    let responses: Vec<_> = recorder
        .all()
        .into_iter()
        .filter(|msg| msg.get("method").is_none())
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], "req-101");
    assert_eq!(responses[0]["result"], json!({"decision": "allowed-once"}));
    assert_eq!(
        lines(&output).len(),
        1,
        "unselected requests stay pending without stdout events"
    );
}

#[test]
fn ongoing_thread_events_cannot_extend_the_hard_timeout() {
    let port = start_mock(base_handler(
        Recorder::default(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({"turnId": "U1"}));
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for seq in 0..20 {
                        std::thread::sleep(Duration::from_millis(100));
                        notify(
                            &tx,
                            "thread/event",
                            json!({"threadId": "T1", "seq": seq, "type": "step/start", "data": {}}),
                        );
                    }
                });
            }
        },
    ));
    let output = run(
        port,
        &[
            "run",
            "--cwd",
            CWD,
            "--prompt",
            "hello",
            "--stall-secs",
            "1",
            "--timeout-secs",
            "1",
        ],
    );
    assert_eq!(code(&output), 3, "{output:?}");
    assert_eq!(lines(&output)[1]["event"], "timeout");
}

#[test]
fn integer_turn_ids_are_accepted_and_echoed_back_as_integers() {
    // The live plugin numbers turns 1, 2, 3 even though the protocol document
    // spells every id as a string.
    let port = start_mock(Arc::new(move |msg, tx| {
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/start" => ok(tx, msg, json!({ "threadId": "T1", "title": "dsh-T1" })),
            "thread/read" => ok(
                tx,
                msg,
                thread_value("T1", idle(), json!([{ "turnId": 7, "status": "running" }])),
            ),
            "turn/start" => {
                ok(tx, msg, json!({ "turnId": 7 }));
                notify(tx, "turn/started", json!({ "threadId": "T1", "turnId": 7 }));
                notify(
                    tx,
                    "turn/completed",
                    json!({
                        "threadId": "T1", "turnId": 7,
                        "status": "completed", "finalMessage": "done"
                    }),
                );
            }
            _ => {}
        }
    }));

    let output = run(port, &["run", "--cwd", CWD, "--prompt", "hi"]);
    assert_eq!(code(&output), 0);
    let lines = lines(&output);
    assert_eq!(lines[0]["turnId"], "7", "CLI output normalises to a string");
    assert_eq!(lines[1]["event"], "turn");
    assert_eq!(lines[1]["turnId"], "7");
    assert_eq!(lines[1]["finalMessage"], "done");

    let history = run(port, &["read", "--thread", "T1"]);
    assert_eq!(code(&history), 0);
    assert_eq!(common::lines(&history)[0]["turns"][0]["id"], "7");
}

#[test]
fn steer_and_interrupt_send_integer_turn_ids_back_unquoted() {
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/read" => ok(
                tx,
                msg,
                thread_value(
                    "T1",
                    active(),
                    json!([{ "turnId": 2, "status": "running" }]),
                ),
            ),
            "turn/steer" => ok(tx, msg, json!({"turnId": msg["params"]["expectedTurnId"]})),
            "turn/interrupt" => ok(tx, msg, json!({})),
            _ => {}
        }
    }));

    let steered = run(port, &["steer", "--thread", "T1", "--text", "go on"]);
    assert_eq!(code(&steered), 0);
    assert_eq!(lines(&steered)[0]["result"]["turnId"], json!(2));
    // `optionalNumber` in the plugin rejects a quoted id with invalid_params.
    assert_eq!(
        recorder.requests("turn/steer")[0]["params"]["expectedTurnId"],
        json!(2)
    );

    let reads_before_explicit = recorder.requests("thread/read").len();
    let explicit = run(
        port,
        &[
            "steer",
            "--thread",
            "T1",
            "--turn",
            "2",
            "--text",
            "use the explicit turn",
        ],
    );
    assert_eq!(code(&explicit), 0, "{explicit:?}");
    assert_eq!(
        recorder.requests("thread/read").len(),
        reads_before_explicit
    );
    assert_eq!(
        recorder.requests("turn/steer")[1]["params"],
        json!({
            "threadId": "T1", "expectedTurnId": 2, "text": "use the explicit turn"
        })
    );
    assert_eq!(lines(&explicit)[0]["result"]["turnId"], json!(2));

    let interrupted = run(port, &["interrupt", "--thread", "T1"]);
    assert_eq!(code(&interrupted), 0);
    assert_eq!(
        recorder.requests("turn/interrupt")[0]["params"]["turnId"],
        json!(2)
    );
    assert_eq!(lines(&interrupted)[0]["turnId"], "2");
}
