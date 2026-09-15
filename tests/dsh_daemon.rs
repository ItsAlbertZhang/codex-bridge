//! Daemon management: the readiness probe, the detached spawn, and the refusal
//! to stop a server this tool did not start.
//!
//! The `node` executable is always a fake script that records its arguments
//! and then exits or lingers, so no real dsh process is ever started here.

mod common;

use std::process::Output;
use std::time::Duration;

use serde_json::{json, Value};

use common::dsh::{
    base_handler, cli, code, dead_port, fake_late_web_node, fake_marker_node, fake_two_web_node,
    gated_mock, idle, lines, marker_readyz, ok, start_mock, Recorder, TempDir,
};

fn node_args(state: &TempDir) -> Option<Vec<String>> {
    std::fs::read_to_string(state.join("node-args.txt"))
        .ok()
        .map(|text| text.lines().map(str::to_string).collect())
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// The note `daemon start` / `restart` print when no fresh URL turns up.
const NO_FRESH_URL: &str = "no new `dsh web:` line";

fn thread_handler() -> common::Handler {
    let recorder = Recorder::default();
    base_handler(recorder, "T1", idle(), |msg, tx, method| {
        if method == "thread/resume" {
            ok(
                tx,
                msg,
                json!({
                    "threadId": "T1",
                    "title": "dsh-T1",
                    "cwd": env!("CARGO_MANIFEST_DIR"),
                    "status": "idle",
                    "pendingRequests": [],
                }),
            );
        }
    })
}

fn mock_with_a_thread() -> (u16, Recorder) {
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
                    json!({
                        "threadId": "T1",
                        "title": "dsh-T1",
                        "cwd": env!("CARGO_MANIFEST_DIR"),
                        "status": "idle",
                        "pendingRequests": [],
                    }),
                );
            }
        },
    ));
    (port, recorder)
}

#[test]
fn an_unready_server_is_spawned_and_the_wait_is_bounded() {
    // Nobody listens here, and the fake node never will either.
    let port = dead_port();
    let state = TempDir::new("spawn");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "600")
        .args(["status", "--thread", "T1"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 4, "a server that never comes up is exit 4");
    let error = &lines(&output)[0];
    assert_eq!(error["event"], "error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains(&format!("http://127.0.0.1:{port}/readyz")),
        "got: {}",
        error["message"]
    );

    let args = node_args(&state).expect("the fake node must have been invoked");
    assert_eq!(
        args,
        [
            state.join("mock dsh bin.js").to_string_lossy().as_ref(),
            "--profile",
            "bridge",
            "--port",
            "12899",
        ],
        "the explicit entry point has exactly these arguments, without --no-open"
    );
    assert!(
        state.join("dsh/daemon.pid").exists(),
        "the spawned pid is recorded"
    );
}

#[test]
fn daemon_status_reports_a_ready_but_unmanaged_server() {
    let (port, _recorder) = mock_with_a_thread();
    let state = TempDir::new("status");

    let output = cli(port, &state)
        .args(["daemon", "status"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    let status = &lines(&output)[0];
    assert_eq!(status["url"], format!("ws://127.0.0.1:{port}"));
    assert_eq!(status["uiUrl"], "http://127.0.0.1:12899/");
    assert_eq!(status["ready"], true);
    assert_eq!(status["managed"], false);
    assert_eq!(status.get("pid"), None);
    assert_eq!(node_args(&state), None, "status never starts anything");
}

#[test]
fn daemon_spawn_passes_the_bin_profile_and_ui_port_overrides() {
    let port = dead_port();
    let state = TempDir::new("spawn-overrides");
    let dsh = state.join("custom dsh bin.js");
    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_BIN", &dsh)
        .env("AGENT_BRIDGE_DSH_PROFILE", "custom bridge")
        .env("AGENT_BRIDGE_DSH_UI_PORT", "18081")
        .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "600")
        .args(["daemon", "start"])
        .output()
        .expect("running agent-bridge with fake node");

    assert_eq!(code(&output), 4);
    assert_eq!(
        node_args(&state).expect("the fake node was invoked"),
        [
            dsh.to_string_lossy().to_string(),
            "--profile".to_string(),
            "custom bridge".to_string(),
            "--port".to_string(),
            "18081".to_string(),
        ]
    );
}

#[test]
fn an_unset_or_empty_dsh_entry_point_uses_the_path_launcher() {
    for empty in [false, true] {
        let state = TempDir::new("dsh path launcher");
        let launcher = state.join(if cfg!(windows) { "dsh.cmd" } else { "dsh" });
        std::fs::copy(common::fake_node(&state), &launcher).unwrap();
        // A relative PATH entry must resolve before the daemon changes cwd.
        let path = if empty {
            state
                .path()
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap()
        } else {
            state.path()
        };
        let mut command = cli(dead_port(), &state);
        command
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("PATH", path)
            .env("AGENT_BRIDGE_DSH_PROFILE", "path bridge")
            .env("AGENT_BRIDGE_DSH_UI_PORT", "18090")
            .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "1");
        if empty {
            command.env("AGENT_BRIDGE_DSH_BIN", "");
        } else {
            command.env_remove("AGENT_BRIDGE_DSH_BIN");
        }
        let output = command.args(["daemon", "start"]).output().unwrap();
        assert_eq!(code(&output), 4, "the fake never becomes ready: {output:?}");
        assert!(lines(&output)[0]["message"]
            .as_str()
            .unwrap()
            .contains("/readyz"));
        assert_eq!(
            node_args(&state).expect("the PATH launcher ran directly"),
            ["--profile", "path bridge", "--port", "18090"]
        );
    }
}

#[test]
fn a_missing_path_launcher_reports_how_to_configure_dsh() {
    let state = TempDir::new("missing-dsh-launcher");
    let output = cli(dead_port(), &state)
        .env_remove("AGENT_BRIDGE_DSH_BIN")
        .env("PATH", state.path())
        .args(["daemon", "start"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 4);
    let events = lines(&output);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "error");
    let message = events[0]["message"].as_str().unwrap();
    assert!(message.contains("dsh was not found on PATH"), "{message}");
    assert!(message.contains("set AGENT_BRIDGE_DSH_BIN"), "{message}");
    assert_eq!(node_args(&state), None);
    assert!(!state.join("dsh/daemon.pid").exists());
}

#[test]
fn node_override_precedes_path_and_empty_override_falls_back_to_path() {
    for setting in [None, Some(""), Some("explicit")] {
        let state = TempDir::new("node-override-precedence");
        let explicit = common::fake_node(&state);
        let fallback = common::fake_cwd(&state);
        let mut command = cli(dead_port(), &state);
        command
            .env("PATH", common::node_path(&fallback))
            .env("FAKE_CWD_LOG", state.join("path-node-cwd.txt"))
            .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "600");
        match setting {
            None => {
                command.env_remove("AGENT_BRIDGE_DSH_NODE_BIN");
            }
            Some("") => {
                command.env("AGENT_BRIDGE_DSH_NODE_BIN", "");
            }
            Some(_) => {
                command.env("AGENT_BRIDGE_DSH_NODE_BIN", &explicit);
            }
        }
        let output = command.args(["daemon", "start"]).output().unwrap();
        assert_eq!(code(&output), 4, "the fake never becomes ready: {output:?}");
        assert_eq!(
            node_args(&state).is_some(),
            setting == Some("explicit"),
            "nonempty explicit NODE_BIN takes precedence over PATH"
        );
        assert_eq!(
            state.join("path-node-cwd.txt").exists(),
            setting != Some("explicit"),
            "unset or empty NODE_BIN uses the fake node on PATH"
        );
    }
}

#[test]
fn daemon_status_uses_the_last_url_in_the_log() {
    let (port, _recorder) = mock_with_a_thread();
    let state = TempDir::new("status-log-url");
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:18081/?token=old\r\n\
         unrelated line\r\n\
         ready: dsh web: http://localhost:18082/?token=new trailing output\r\n\
         dsh web: https://ignored.example/\r\n",
    )
    .expect("daemon log");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_UI_PORT", "18083")
        .args(["daemon", "status"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    assert_eq!(
        lines(&output)[0]["uiUrl"],
        "http://localhost:18082/?token=new"
    );
    assert_eq!(node_args(&state), None, "status never starts anything");
}

#[test]
fn daemon_status_falls_back_to_the_configured_ui_port_without_a_match() {
    let port = dead_port();
    let state = TempDir::new("status-fallback-url");
    std::fs::write(state.join("dsh/daemon.log"), "dsh starting\n").expect("daemon log");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_UI_PORT", "18084")
        .args(["daemon", "status"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    assert_eq!(lines(&output)[0]["uiUrl"], "http://127.0.0.1:18084/");
    assert_eq!(lines(&output)[0]["ready"], false);
    assert_eq!(node_args(&state), None, "status never starts anything");
}

#[test]
fn daemon_start_waits_for_a_new_web_url_written_after_the_spawn() {
    // The fake opens the readiness gate before delaying its URL. This forces
    // the spawn path independently of how quickly the CLI process starts.
    let state = TempDir::new("start-fresh-url");
    let gate = state.join("readyz-gate");
    let port = gated_mock(thread_handler(), Some(gate.clone()));
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:12899/?token=OLD\r\n",
    )
    .expect("daemon log");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_NODE_BIN", fake_late_web_node(&state))
        .env("FAKE_NODE_MARKER", &gate)
        .args(["daemon", "start"])
        .output()
        .expect("running agent-bridge with fake node");

    assert_eq!(code(&output), 0);
    assert_eq!(
        lines(&output)[0]["uiUrl"],
        "http://127.0.0.1:12899/?token=NEW"
    );
}

#[test]
fn daemon_start_on_an_already_ready_server_does_not_wait_for_a_new_url() {
    let (port, _recorder) = mock_with_a_thread();
    let state = TempDir::new("start-ready-url");
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:12899/?token=OLD\r\n",
    )
    .expect("daemon log");

    let start = std::time::Instant::now();
    let output = cli(port, &state)
        .args(["daemon", "start"])
        .output()
        .expect("running agent-bridge");
    let elapsed = start.elapsed();

    assert_eq!(code(&output), 0);
    assert_eq!(
        lines(&output)[0]["uiUrl"],
        "http://127.0.0.1:12899/?token=OLD"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "nothing was spawned, so there is no 5 s URL wait: {elapsed:?}"
    );
    assert_eq!(node_args(&state), None, "readyz was 200: nothing to start");
}

#[test]
fn daemon_start_takes_the_first_url_written_after_the_spawn_not_the_last() {
    // Both of the child's URLs are in the log before its port answers, so the
    // only thing that picks NEW1 out is an offset taken before the spawn plus a
    // scan that takes the first match: an offset recorded after readiness sees
    // an empty tail and falls back to the last line, and a scan that takes the
    // last match reports NEW2 outright.
    let state = TempDir::new("start-first-url");
    let gate = state.join("readyz-gate");
    let port = gated_mock(thread_handler(), Some(gate.clone()));
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:12899/?token=OLD\r\n",
    )
    .expect("daemon log");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_NODE_BIN", fake_two_web_node(&state))
        .env("FAKE_NODE_MARKER", &gate)
        .args(["daemon", "start"])
        .output()
        .expect("running agent-bridge with fake node");

    assert_eq!(code(&output), 0);
    assert_eq!(
        lines(&output)[0]["uiUrl"],
        "http://127.0.0.1:12899/?token=NEW1"
    );
}

#[test]
fn daemon_start_falls_back_to_the_last_url_and_says_so_on_stderr() {
    // The fake node opens its port and then exits without ever printing a
    // `dsh web:` line, so the wait runs out.
    let state = TempDir::new("start-url-timeout");
    let gate = state.join("readyz-gate");
    let port = gated_mock(thread_handler(), Some(gate.clone()));
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:12899/?token=OLD\r\n",
    )
    .expect("daemon log");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_NODE_BIN", fake_marker_node(&state, None))
        .env("FAKE_NODE_MARKER", &gate)
        .env("AGENT_BRIDGE_DSH_UI_URL_WAIT_MS", "400")
        .args(["daemon", "start"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    assert_eq!(
        lines(&output)[0]["uiUrl"],
        "http://127.0.0.1:12899/?token=OLD"
    );
    let note = stderr(&output);
    assert!(note.contains(NO_FRESH_URL), "got: {note}");
    assert!(
        note.contains("400 ms"),
        "the configured budget is named: {note}"
    );
}

#[test]
fn a_thread_command_does_not_wait_for_a_new_url_on_the_spawn_path() {
    // The mock refuses readiness until the fake node opens the gate, so the
    // first check fails and the CLI takes the spawn path. That node never
    // prints a `dsh web:` line, and only `daemon start` and `daemon restart`
    // may pay for that: this command has to return as soon as the server
    // answers, with neither the URL wait nor its stderr note.
    let state = TempDir::new("no-url-wait");
    let gate = state.join("readyz-gate");
    let port = gated_mock(thread_handler(), Some(gate.clone()));

    let start = std::time::Instant::now();
    let output = cli(port, &state)
        .env("AGENT_BRIDGE_DSH_NODE_BIN", fake_marker_node(&state, None))
        .env("FAKE_NODE_MARKER", &gate)
        .args(["status", "--thread", "T1"])
        .output()
        .expect("running agent-bridge");
    let elapsed = start.elapsed();

    assert_eq!(code(&output), 0);
    assert!(
        node_args(&state).is_some(),
        "the spawn path must have been taken"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "a thread command never waits 5 s for a URL it does not print: {elapsed:?}"
    );
    let note = stderr(&output);
    assert!(!note.contains(NO_FRESH_URL), "got: {note}");
}

#[test]
fn daemon_restart_waits_for_the_old_server_to_stop_answering() {
    // The old daemon keeps answering `/readyz` for 700 ms after it is killed.
    // `restart` must wait that out: a `bring_up` that probes straight after
    // `stop` finds a ready server, spawns nothing, and reports the old token.
    let state = TempDir::new("restart");
    let old = std::process::Command::new(common::fake_lingering_node(&state))
        .env("FAKE_NODE_LOG", state.join("old-args.txt"))
        .spawn()
        .expect("spawning the old daemon stand-in");
    let old_pid = old.id();
    std::fs::write(state.join("dsh/daemon.pid"), old_pid.to_string()).expect("pid file");
    std::fs::write(
        state.join("dsh/daemon.log"),
        "dsh web: http://127.0.0.1:12899/?token=OLD\r\n",
    )
    .expect("daemon log");

    let lingering = state.join("old-readyz-marker");
    let fresh = state.join("new-readyz-marker");
    std::fs::write(&lingering, "ready").expect("marker");
    let port = marker_readyz(lingering, fresh.clone(), old, Duration::from_millis(700));

    let output = cli(port, &state)
        .env(
            "AGENT_BRIDGE_DSH_NODE_BIN",
            fake_marker_node(&state, Some("http://127.0.0.1:12899/?token=NEW")),
        )
        .env("FAKE_NODE_MARKER", &fresh)
        .args(["daemon", "restart"])
        .output()
        .expect("running agent-bridge with fake node");

    assert_eq!(code(&output), 0);
    let events = lines(&output);
    assert_eq!(events[0]["event"], "stopped");
    assert_eq!(events[0]["pid"], Value::from(old_pid));
    assert!(
        node_args(&state).is_some(),
        "restart brings up a new daemon instead of finding the old one ready"
    );
    assert_eq!(events[1]["ready"], true);
    assert_eq!(
        events[1]["uiUrl"], "http://127.0.0.1:12899/?token=NEW",
        "the new process' token, not the one already in the log"
    );
}
