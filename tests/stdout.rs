//! Shared output failure behavior, using only scratch paths and protocol mocks.

mod common;

use common::{base_handler, cli, idle, notify, ok, start_mock, Recorder, TempDir, Tx};
use serde_json::{json, Value};
use std::io::pipe;
use std::process::{Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const THREAD_ID: &str = "T-closed-stdout";

fn assert_stdout_failure(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("agent-bridge: failed writing stdout: "),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("backtrace"), "{stderr}");
}

fn closed_stdout() -> Stdio {
    let (reader, writer) = pipe().expect("stdout pipe");
    drop(reader);
    Stdio::from(writer)
}

#[tokio::test]
async fn closing_stdout_after_started_exits_cleanly_on_turn_completion() {
    let (pending, completion) = mpsc::channel::<Tx>();
    let port = start_mock(base_handler(
        Recorder::default(),
        THREAD_ID,
        idle(),
        move |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
                pending.send(tx.clone()).expect("completion sender");
            }
        },
    ));
    let state = TempDir::new("stdout-turn");
    let mut command = cli(port, &state);
    command
        .args(["--no-pane", "run", "--cwd"])
        .arg(state.path())
        .args(["--prompt", "hi", "--timeout-secs", "10"]);
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running agent-bridge");

    let mut stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
    let mut first = String::new();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut first))
        .await
        .expect("started arrives before turn completion")
        .expect("reading started");
    let started: Value = serde_json::from_str(&first).expect("started JSON");
    assert_eq!(started["event"], "started");
    drop(stdout);

    let tx = completion
        .recv_timeout(Duration::from_secs(1))
        .expect("turn completion can now be delivered");
    notify(
        &tx,
        "turn/completed",
        json!({
            "threadId": THREAD_ID,
            "turn": { "id": "U1", "status": "completed" }
        }),
    );
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("CLI exits when stdout is closed")
        .expect("waiting for agent-bridge");
    assert_stdout_failure(&output);
}

#[test]
fn daemon_output_uses_the_same_closed_stdout_exit() {
    let state = TempDir::new("stdout-daemon");
    let port = start_mock(std::sync::Arc::new(|_, _| {}));
    let output = cli(port, &state)
        .args(["daemon", "status"])
        .stdout(closed_stdout())
        .output()
        .expect("running daemon status");
    assert_stdout_failure(&output);
}

#[test]
fn error_events_do_not_retry_a_closed_stdout_or_print_a_second_diagnostic() {
    let state = TempDir::new("stdout-error");
    let config = state.join("invalid-config.toml");
    std::fs::write(&config, "[invalid").expect("invalid test configuration");
    let output = cli(common::dead_port(), &state)
        .env("AGENT_BRIDGE_CONFIG", config)
        .args(["daemon", "status"])
        .stdout(closed_stdout())
        .output()
        .expect("running with an invalid configuration");
    assert_stdout_failure(&output);
}

#[test]
fn help_and_version_fail_cleanly_when_stdout_is_closed() {
    for flag in ["--help", "--version"] {
        let state = TempDir::new("stdout-clap");
        let output = cli(common::dead_port(), &state)
            .arg(flag)
            .stdout(closed_stdout())
            .output()
            .expect("running help or version");
        assert_stdout_failure(&output);
    }
}
