//! These tests cover the shared core through the Codex mock and include Codex-specific cases.
//! Home-directory coverage also exercises the dsh launcher.
//! Daemon management: the readiness probe, the detached spawn, and the refusal
//! to stop a server this tool did not start.
//!
//! The `codex` executable is always a fake script that records its arguments
//! and then exits or lingers, so no real app-server is ever started here.

mod common;

use std::io::Read;
use std::process::{Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use common::{base_handler, cli, code, dead_port, idle, lines, ok, start_mock, Recorder, TempDir};

fn codex_args(state: &TempDir) -> Option<String> {
    std::fs::read_to_string(state.join("codex-args.txt")).ok()
}

fn mock_with_a_thread() -> (u16, Recorder) {
    let recorder = Recorder::default();
    let port = start_mock(base_handler(
        recorder.clone(),
        "T1",
        idle(),
        |msg, tx, method| {
            if method == "thread/resume" {
                ok(tx, msg, json!({ "thread": { "id": "T1" } }));
            }
        },
    ));
    (port, recorder)
}

#[test]
fn a_ready_server_is_never_spawned_again() {
    let (port, _recorder) = mock_with_a_thread();
    let state = TempDir::new("ready");

    let output: Output = cli(port, &state)
        .args(["status", "--thread", "T1"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    assert_eq!(codex_args(&state), None, "readyz was 200: nothing to start");
    assert!(
        !state.join("codex/daemon.pid").exists(),
        "no pid file without a spawn"
    );
}

// Codex-specific: the app-server --listen launcher arguments.
#[test]
fn an_unready_server_is_spawned_and_the_wait_is_bounded() {
    // Nobody listens here, and the fake codex never will either.
    let port = dead_port();
    let state = TempDir::new("spawn");

    let output = cli(port, &state)
        .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "600")
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

    let args = codex_args(&state).expect("the fake codex must have been invoked");
    assert!(
        args.contains(&format!("app-server --listen ws://127.0.0.1:{port}")),
        "got: {args}"
    );
    assert!(
        state.join("codex/daemon.pid").exists(),
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
    assert_eq!(status["ready"], true);
    assert_eq!(status["managed"], false);
    assert_eq!(status.get("pid"), None);
    assert_eq!(codex_args(&state), None, "status never starts anything");
}

#[test]
fn daemon_stop_refuses_a_server_it_does_not_manage() {
    let (port, _recorder) = mock_with_a_thread();
    let state = TempDir::new("stop-unmanaged");

    let output = cli(port, &state)
        .args(["daemon", "stop"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 4);
    let error = &lines(&output)[0];
    assert_eq!(error["event"], "error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("is not managed by agent-bridge"),
        "got: {}",
        error["message"]
    );
}

#[test]
fn daemon_stop_clears_a_stale_pid_file() {
    let (port, _recorder) = mock_with_a_thread();
    let state = TempDir::new("stop-stale");
    // A pid that is certainly dead: run the fake to exhaustion and reuse its own.
    let mut corpse = std::process::Command::new(common::fake_codex(&state))
        .env("FAKE_CODEX_LOG", state.join("corpse.txt"))
        .spawn()
        .expect("spawning the fake");
    corpse.wait().expect("the fake exits at once");
    let pid = corpse.id();
    std::fs::write(state.join("codex/daemon.pid"), pid.to_string()).expect("pid file");

    let output = cli(port, &state)
        .args(["daemon", "stop"])
        .output()
        .expect("running agent-bridge");

    assert_eq!(code(&output), 0);
    let stopped = &lines(&output)[0];
    assert_eq!(stopped["event"], "stopped");
    assert_eq!(stopped["pid"], Value::from(pid));
    assert!(
        !state.join("codex/daemon.pid").exists(),
        "the pid file is gone"
    );
}

#[test]
fn a_spawned_daemon_does_not_hold_the_callers_pipes_open() {
    // The daemon outlives the CLI. If it inherits the CLI's standard handles,
    // whoever captured the CLI's stdout (a `$(...)` substitution, a parent
    // program) never sees EOF and hangs for as long as the daemon lives.
    let port = dead_port();
    let state = TempDir::new("pipes");

    let mut child = cli(port, &state)
        .env(
            "AGENT_BRIDGE_CODEX_BIN",
            common::fake_lingering_codex(&state),
        )
        .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "600")
        .args(["status", "--thread", "T1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running agent-bridge");

    let mut stdout = child.stdout.take().expect("the stdout pipe");
    let mut stderr = child.stderr.take().expect("the stderr pipe");
    let status = child.wait().expect("the CLI exits on its own");
    assert_eq!(status.code(), Some(4), "the daemon never became ready");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let read = stdout
            .read_to_end(&mut out)
            .and_then(|_| stderr.read_to_end(&mut err));
        let _ = tx.send(read.map(|_| (out, err)));
    });
    let (out, _err) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("both pipes reach EOF within 1 s of the CLI's exit")
        .expect("reading the CLI's pipes");

    let error: Value = serde_json::from_str(
        String::from_utf8_lossy(&out)
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("one event on stdout"),
    )
    .expect("the event is JSON");
    assert_eq!(error["event"], "error");
    assert!(
        codex_args(&state).is_some(),
        "the spawn path must have been taken"
    );

    let pid: u32 = std::fs::read_to_string(state.join("codex/daemon.pid"))
        .expect("the pid file")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        still_running(pid),
        "the daemon outlives the CLI: EOF came from the handles, not from its death"
    );
    kill(pid);
}

#[cfg(windows)]
fn still_running(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

#[test]
fn daemon_restart_waits_for_the_old_server_to_stop_answering() {
    let state = TempDir::new("restart-shared");
    let old = std::process::Command::new(common::fake_lingering_codex(&state))
        .env("FAKE_CODEX_LOG", state.join("old-args.txt"))
        .spawn()
        .expect("spawning the old daemon stand-in");
    let old_pid = old.id();
    std::fs::write(state.join("codex/daemon.pid"), old_pid.to_string()).expect("pid file");
    let lingering = state.join("old-readyz-marker");
    let fresh = state.join("new-readyz-marker");
    std::fs::write(&lingering, "ready").expect("old ready marker");
    let port = common::marker_readyz(lingering, fresh.clone(), old, Duration::from_millis(700));

    let started = std::time::Instant::now();
    let output = cli(port, &state)
        .env("AGENT_BRIDGE_CODEX_BIN", common::fake_marker_codex(&state))
        .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "4000")
        .env("FAKE_CODEX_MARKER", &fresh)
        .args(["daemon", "restart"])
        .output()
        .expect("running restart against fake daemon");

    assert_eq!(code(&output), 0, "{output:?}");
    assert!(started.elapsed() >= Duration::from_millis(700));
    let events = lines(&output);
    assert_eq!(events[0]["event"], "stopped");
    assert_eq!(events[0]["pid"], old_pid);
    assert_eq!(events[1]["ready"], true);
    assert!(fresh.exists(), "the replacement launcher must run");
    assert!(codex_args(&state).is_some());
}

#[test]
fn spawned_daemons_use_the_user_home_directory() {
    for backend in ["codex", "dsh"] {
        let state = TempDir::new("daemon-home");
        let home = state.join("user home");
        let caller = state.join("caller cwd");
        std::fs::create_dir(&home).expect("home");
        std::fs::create_dir(&caller).expect("caller cwd");
        let script = common::fake_cwd(&state);
        let mut command = if backend == "codex" {
            let mut command = cli(dead_port(), &state);
            command.env("AGENT_BRIDGE_CODEX_BIN", &script);
            command
        } else {
            let mut command = common::dsh::cli(dead_port(), &state);
            command.env("AGENT_BRIDGE_DSH_NODE_BIN", &script);
            command
        };
        let output = command
            .current_dir(&caller)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env(
                format!("AGENT_BRIDGE_{}_READY_TIMEOUT_MS", backend.to_uppercase()),
                "600",
            )
            .env("FAKE_CWD_LOG", state.join("cwd.txt"))
            .args(["daemon", "start"])
            .output()
            .expect("running fake daemon");
        assert_eq!(code(&output), 4, "the recording fake never becomes ready");
        let cwd = std::fs::read_to_string(state.join("cwd.txt")).expect("recorded cwd");
        assert_eq!(
            std::path::Path::new(cwd.trim()).canonicalize().unwrap(),
            home.canonicalize().unwrap(),
            "{backend} daemon must use the selected home"
        );
    }
}

#[test]
fn a_missing_home_falls_back_to_the_backend_state_directory() {
    let state = TempDir::new("daemon-home-fallback");
    let output = cli(dead_port(), &state)
        .env_remove("USERPROFILE")
        .env_remove("HOME")
        .env("AGENT_BRIDGE_CODEX_BIN", common::fake_cwd(&state))
        .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "600")
        .env("FAKE_CWD_LOG", state.join("cwd.txt"))
        .args(["daemon", "start"])
        .output()
        .expect("running fake daemon without a home");
    assert_eq!(code(&output), 4);
    let cwd = std::fs::read_to_string(state.join("cwd.txt")).expect("recorded cwd");
    assert_eq!(
        std::path::Path::new(cwd.trim()).canonicalize().unwrap(),
        state.join("codex").canonicalize().unwrap()
    );
}

#[cfg(windows)]
fn kill(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
}

#[cfg(unix)]
fn still_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}
