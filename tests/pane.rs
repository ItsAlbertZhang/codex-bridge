//! The Herdr pane. `herdr` is always a fake script that records its arguments,
//! so no real pane is ever opened here.

mod common;

use std::path::PathBuf;
use std::process::Output;

use serde_json::json;

use common::{base_handler, cli, code, fake_herdr, idle, lines, ok, start_mock, Recorder, TempDir};

/// A mock that starts thread `T1` and answers one turn, plus the fake herdr.
struct Fixture {
    port: u16,
    state: TempDir,
    herdr: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let recorder = Recorder::default();
        let port = start_mock(base_handler(recorder, "T1", idle(), |msg, tx, method| {
            if method == "turn/start" {
                ok(tx, msg, json!({ "turn": { "id": "U1" } }));
            }
        }));
        let state = TempDir::new(tag);
        let herdr = fake_herdr(&state);
        Self { port, state, herdr }
    }

    /// `run --no-wait` inside a Herdr session, with `agent get` answering `get`.
    fn run(&self, agent_get: &str, extra: &[(&str, &str)]) -> Output {
        let mut command = cli(self.port, &self.state);
        command
            .env("HERDR_ENV", "1")
            .env("AGENT_BRIDGE_CODEX_HERDR_BIN", &self.herdr)
            .env("AGENT_BRIDGE_CODEX_PANE_DELAY_MS", "0")
            .env("AGENT_BRIDGE_CODEX_PANE_RETRY_MS", "0")
            .env("FAKE_HERDR_LOG", self.state.join("herdr-args.txt"))
            .env(
                "FAKE_HERDR_RENAME_COUNT",
                self.state.join("rename-count.txt"),
            )
            .env("FAKE_HERDR_AGENT_GET", agent_get);
        for (key, value) in extra {
            command.env(key, value);
        }
        command
            .args(["run", "--cwd", "/tmp", "--prompt", "hi", "--no-wait"])
            .output()
            .expect("running agent-bridge")
    }

    fn herdr_log(&self) -> Option<String> {
        std::fs::read_to_string(self.state.join("herdr-args.txt")).ok()
    }
}

/// A `herdr pane list` answer: one pane per `(pane_id, label)`, plus one that
/// carries no label at all, the way herdr omits the field for unnamed panes.
fn pane_list(panes: &[(&str, &str)]) -> String {
    let mut listed = vec![json!({ "pane_id": "PX" })];
    listed.extend(
        panes
            .iter()
            .map(|(id, label)| json!({ "pane_id": id, "label": label })),
    );
    json!({ "result": { "type": "pane_list", "panes": listed } }).to_string()
}

/// How many recorded invocations start with `prefix`.
fn calls(log: &str, prefix: &str) -> usize {
    log.lines().filter(|l| l.trim().starts_with(prefix)).count()
}

/// `run` must print exactly the started event and exit 0, pane or no pane.
fn assert_started(output: &Output) {
    assert_eq!(code(output), 0);
    let lines = lines(output);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["event"], "started");
    assert_eq!(lines[0]["threadId"], "T1");
}

#[test]
fn a_pane_is_opened_when_no_agent_is_attached_yet() {
    let fixture = Fixture::new("pane-open");
    let output = fixture.run("1", &[]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent get codex-T1"), "got: {log}");
    assert!(
        log.contains("pane split --current --direction right --ratio 0.45 --cwd /tmp"),
        "got: {log}"
    );
    assert!(
        log.contains(&format!(
            "agent start codex-T1 --kind codex --pane P9 --timeout 5000 -- resume T1 --remote ws://127.0.0.1:{}",
            fixture.port
        )),
        "got: {log}"
    );
    assert!(
        log.contains("pane rename P9 codex-T1"),
        "the pane carries the name even after the TUI goes: {log}"
    );
    assert!(
        log.contains("agent rename P9 codex-T1"),
        "the name is bound to the pane: {log}"
    );
    assert_eq!(calls(&log, "agent rename"), 1, "one rename suffices: {log}");
}

#[test]
fn a_pane_that_outlived_its_agent_is_reused_instead_of_split() {
    let fixture = Fixture::new("pane-relabel");
    let listed = pane_list(&[("P4", "codex-T1")]);
    let output = fixture.run("1", &[("FAKE_HERDR_PANE_LIST", listed.as_str())]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("pane list"), "got: {log}");
    assert!(
        !log.contains("pane split"),
        "the named pane is still there, so nothing is split: {log}"
    );
    assert!(
        log.contains(&format!(
            "agent start codex-T1 --kind codex --pane P4 --timeout 5000 -- resume T1 --remote ws://127.0.0.1:{}",
            fixture.port
        )),
        "codex goes back into the pane that kept the name: {log}"
    );
    assert!(
        log.contains("agent rename P4 codex-T1"),
        "and the name is bound again: {log}"
    );
}

#[test]
fn a_pane_list_without_the_name_still_splits() {
    let fixture = Fixture::new("pane-list-miss");
    let listed = pane_list(&[("P4", "codex-OTHER")]);
    let output = fixture.run("1", &[("FAKE_HERDR_PANE_LIST", listed.as_str())]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("pane list"), "got: {log}");
    assert!(
        log.contains("pane split --current"),
        "no pane answers to the name, so one is split: {log}"
    );
    assert!(log.contains("pane rename P9 codex-T1"), "got: {log}");
    assert!(log.contains("agent start codex-T1"), "got: {log}");
}

#[test]
fn a_reused_pane_that_refuses_both_calls_does_not_split_a_second_one() {
    let fixture = Fixture::new("pane-relabel-busy");
    let listed = pane_list(&[("P4", "codex-T1")]);
    let output = fixture.run(
        "1",
        &[
            ("FAKE_HERDR_PANE_LIST", listed.as_str()),
            ("FAKE_HERDR_START_FAIL", "1"),
            ("FAKE_HERDR_RENAME_FAILS", "99"),
        ],
    );
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(
        !log.contains("pane split"),
        "a busy pane is still this thread's pane: {log}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent start did not confirm readiness"),
        "got: {stderr}"
    );
    assert!(stderr.contains("agent rename"), "got: {stderr}");
}

#[test]
fn a_start_that_never_confirms_still_binds_the_name() {
    let fixture = Fixture::new("pane-start-timeout");
    let output = fixture.run("1", &[("FAKE_HERDR_START_FAIL", "1")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent start codex-T1"), "got: {log}");
    assert!(
        log.contains("agent rename P9 codex-T1"),
        "the rename target is the pane the split returned: {log}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("agent start did not confirm readiness"),
        "the start failure belongs on stderr"
    );
}

#[test]
fn a_rename_that_fails_at_first_is_retried() {
    let fixture = Fixture::new("pane-rename-retry");
    let output = fixture.run("1", &[("FAKE_HERDR_RENAME_FAILS", "3")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert_eq!(
        calls(&log, "agent rename"),
        4,
        "three failures then the one that sticks: {log}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("agent rename"),
        "a rename that eventually works says nothing: {stderr}"
    );
}

#[test]
fn a_rename_that_never_works_changes_neither_stdout_nor_the_exit_code() {
    let fixture = Fixture::new("pane-rename-fail");
    let output = fixture.run("1", &[("FAKE_HERDR_RENAME_FAILS", "99")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert_eq!(
        calls(&log, "agent rename"),
        11,
        "the first call plus ten retries: {log}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.contains("agent rename"))
            .count(),
        1,
        "exactly one line about it: {stderr}"
    );
}

#[test]
fn an_existing_agent_is_left_alone() {
    let fixture = Fixture::new("pane-reuse");
    let output = fixture.run("0", &[]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("agent get codex-T1"), "got: {log}");
    assert!(
        !log.contains("pane split"),
        "a second pane must not be opened: {log}"
    );
}

#[test]
fn a_failing_pane_changes_neither_stdout_nor_the_exit_code() {
    let fixture = Fixture::new("pane-fail");
    let output = fixture.run("1", &[("FAKE_HERDR_SPLIT_FAIL", "1")]);
    assert_started(&output);

    let log = fixture.herdr_log().expect("herdr must have been called");
    assert!(log.contains("pane split"), "got: {log}");
    assert!(
        !log.contains("agent start"),
        "the split failed, so nothing is attached: {log}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pane split failed"),
        "the failure belongs on stderr"
    );
}

#[test]
fn no_pane_skips_it() {
    let fixture = Fixture::new("pane-off");
    let mut command = cli(fixture.port, &fixture.state);
    let output = command
        .env("HERDR_ENV", "1")
        .env("AGENT_BRIDGE_CODEX_HERDR_BIN", &fixture.herdr)
        .env("AGENT_BRIDGE_CODEX_PANE_DELAY_MS", "0")
        .env("FAKE_HERDR_LOG", fixture.state.join("herdr-args.txt"))
        .env("FAKE_HERDR_AGENT_GET", "1")
        .args([
            "--no-pane",
            "run",
            "--cwd",
            "/tmp",
            "--prompt",
            "hi",
            "--no-wait",
        ])
        .output()
        .expect("running agent-bridge");

    assert_started(&output);
    assert_eq!(fixture.herdr_log(), None, "herdr must not be called at all");
}

#[test]
fn outside_herdr_there_is_no_pane() {
    let fixture = Fixture::new("pane-outside");
    let mut command = cli(fixture.port, &fixture.state);
    // `cli` already removes HERDR_ENV; this is the case where nobody set it.
    let output = command
        .env("AGENT_BRIDGE_CODEX_HERDR_BIN", &fixture.herdr)
        .env("AGENT_BRIDGE_CODEX_PANE_DELAY_MS", "0")
        .env("FAKE_HERDR_LOG", fixture.state.join("herdr-args.txt"))
        .env("FAKE_HERDR_AGENT_GET", "1")
        .args(["run", "--cwd", "/tmp", "--prompt", "hi", "--no-wait"])
        .output()
        .expect("running agent-bridge");

    assert_started(&output);
    assert_eq!(fixture.herdr_log(), None, "herdr must not be called at all");
}
