//! These tests cover the shared core through the Codex mock and include Codex-specific cases.
//! Cross-backend parser and state comparisons never contact a configured real endpoint.

mod common;

use common::{code, dead_port, lines, TempDir};
use serde_json::json;

#[test]
fn help_is_available_at_every_command_layer() {
    let state = TempDir::new("help-layers");
    for backend in ["codex", "dsh"] {
        for verb in [
            None,
            Some("run"),
            Some("wait"),
            Some("reply"),
            Some("steer"),
            Some("interrupt"),
            Some("status"),
            Some("read"),
            Some("daemon"),
        ] {
            let mut command = common::isolated(backend, dead_port(), &state);
            if let Some(verb) = verb {
                command.arg(verb);
            }
            let output = command.arg("--help").output().unwrap();
            assert_eq!(code(&output), 0, "{backend} {verb:?}: {output:?}");
            assert!(!output.stdout.is_empty());
            assert!(output.stderr.is_empty());
        }
    }
    for flag in ["--help", "--version"] {
        let output = std::process::Command::new(common::BIN)
            .arg(flag)
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("agent-bridge"));
        if flag == "--version" {
            assert!(stdout.contains("0.2.0"));
        }
    }
}

// Codex-specific: approval vocabulary, title rejection, and numeric request IDs.
#[test]
fn codex_rejects_dsh_only_arguments_before_connecting() {
    let state = TempDir::new("codex-parser-boundary");
    for args in [
        vec!["run", "--cwd", ".", "--prompt", "hi", "--title", "title"],
        vec!["run", "--cwd", ".", "--prompt", "hi", "--approval", "ask"],
        vec![
            "run",
            "--cwd",
            ".",
            "--prompt",
            "hi",
            "--sandbox",
            "unknown",
        ],
        vec![
            "reply",
            "--thread",
            "T1",
            "--request-id",
            "req-1",
            "--decision",
            "accept",
        ],
    ] {
        let output = common::cli(dead_port(), &state)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(code(&output), 4, "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        assert!(!state.join("codex-args.txt").exists());
    }
}

#[test]
fn the_backend_name_is_required_before_backend_options_or_verbs() {
    for args in [
        vec!["status", "--thread", "T1"],
        vec![
            "--url",
            "ws://invalid/",
            "codex",
            "status",
            "--thread",
            "T1",
        ],
    ] {
        let output = std::process::Command::new(common::BIN)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(code(&output), 4, "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn backend_environment_names_replace_the_old_url_prefixes() {
    let state = TempDir::new("legacy-environment");
    for (backend, prefix, default_port) in [("codex", "CODEX", "12897"), ("dsh", "DSH", "12898")] {
        let legacy_url = format!("ws://127.0.0.1:{}", dead_port());
        // Help exposes clap's selected environment/default value without probing it.
        let output = std::process::Command::new(common::BIN)
            .env_remove(format!("AGENT_BRIDGE_{prefix}_URL"))
            .env(format!("{prefix}_BRIDGE_URL"), &legacy_url)
            .env("AGENT_BRIDGE_STATE_DIR", state.path())
            .args([backend, "--help"])
            .output()
            .unwrap();
        assert_eq!(code(&output), 0);
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(
            help.contains(&format!("AGENT_BRIDGE_{prefix}_URL")),
            "{help}"
        );
        assert!(
            help.contains(&format!("ws://127.0.0.1:{default_port}")),
            "{help}"
        );
        assert!(
            !help.contains(&legacy_url),
            "legacy URL must be ignored: {help}"
        );
    }
}

#[test]
fn identical_thread_ids_have_independent_backend_logs() {
    let state = TempDir::new("backend-isolation");
    let codex_port = common::start_mock(common::base_handler(
        common::Recorder::default(),
        "T-shared",
        common::idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                common::ok(tx, msg, json!({"turn": {"id": "C1"}}));
            }
        },
    ));
    let dsh_port = common::start_mock(common::dsh::base_handler(
        common::Recorder::default(),
        "T-shared",
        common::dsh::idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                common::ok(tx, msg, json!({"turnId": 1}));
            }
        },
    ));
    let codex = common::cli(codex_port, &state)
        .args([
            "run",
            "--cwd",
            env!("CARGO_MANIFEST_DIR"),
            "--prompt",
            "codex",
            "--no-wait",
        ])
        .output()
        .unwrap();
    let dsh = common::dsh::cli(dsh_port, &state)
        .args([
            "run",
            "--cwd",
            env!("CARGO_MANIFEST_DIR"),
            "--prompt",
            "dsh",
            "--no-wait",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&codex), 0, "{codex:?}");
    assert_eq!(code(&dsh), 0, "{dsh:?}");
    let codex_path = state.join("codex/logs/T-shared.jsonl");
    let dsh_path = state.join("dsh/logs/T-shared.jsonl");
    assert_eq!(lines(&codex)[0]["logPath"], json!(codex_path));
    assert_eq!(lines(&dsh)[0]["logPath"], json!(dsh_path));
    assert!(std::fs::read_to_string(codex_path).unwrap().contains("C1"));
    assert!(!std::fs::read_to_string(dsh_path).unwrap().contains("C1"));
    assert!(!state.join("logs").exists());
}

#[test]
fn old_state_overrides_are_ignored_in_favor_of_the_new_default_layout() {
    let state = TempDir::new("legacy-state");
    let port = common::start_mock(common::base_handler(
        common::Recorder::default(),
        "T1",
        common::idle(),
        |msg, tx, method| {
            if method == "turn/start" {
                common::ok(tx, msg, json!({"turn": {"id": "U1"}}));
            }
        },
    ));
    let legacy = state.join("legacy-state-must-stay-unused");
    let output = common::cli(port, &state)
        .env_remove("AGENT_BRIDGE_STATE_DIR")
        .env("CODEX_BRIDGE_STATE_DIR", &legacy)
        .env("DSH_BRIDGE_STATE_DIR", &legacy)
        .env("XDG_STATE_HOME", state.path())
        .args(["run", "--cwd", ".", "--prompt", "hi", "--no-wait"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    let path = state.join("agent-bridge/codex/logs/T1.jsonl");
    assert_eq!(lines(&output)[0]["logPath"], json!(path));
    assert!(path.exists());
    assert!(!legacy.exists());
}

#[test]
fn daemon_pid_defaults_to_the_home_state_directory() {
    for backend in ["codex", "dsh"] {
        let state = TempDir::new("home-state-root");
        let home = state.join("user home");
        std::fs::create_dir(&home).unwrap();
        let mut command = match backend {
            "codex" => common::cli(dead_port(), &state),
            _ => common::dsh::cli(dead_port(), &state),
        };
        let output = command
            .env_remove("AGENT_BRIDGE_STATE_DIR")
            .env_remove("XDG_STATE_HOME")
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "600")
            .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "600")
            .args(["daemon", "start"])
            .output()
            .unwrap();
        assert_eq!(code(&output), 4, "the fake never becomes ready: {output:?}");
        let pid = home
            .join(".local/state/agent-bridge")
            .join(backend)
            .join("daemon.pid");
        assert!(std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse::<u32>()
            .is_ok());
        assert!(!state.join(backend).join("daemon.pid").exists());
    }
}

#[test]
fn xdg_state_home_takes_priority_over_the_user_home() {
    for backend in ["codex", "dsh"] {
        let state = TempDir::new("xdg-state-root");
        let home = state.join("user home");
        let xdg = state.join("xdg state");
        std::fs::create_dir(&home).unwrap();
        let mut command = match backend {
            "codex" => common::cli(dead_port(), &state),
            _ => common::dsh::cli(dead_port(), &state),
        };
        let output = command
            .env("AGENT_BRIDGE_STATE_DIR", "")
            .env("XDG_STATE_HOME", &xdg)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "600")
            .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "600")
            .args(["daemon", "start"])
            .output()
            .unwrap();
        assert_eq!(code(&output), 4, "the fake never becomes ready: {output:?}");
        let pid = xdg.join("agent-bridge").join(backend).join("daemon.pid");
        assert!(std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse::<u32>()
            .is_ok());
        assert!(!home.join(".local/state/agent-bridge").exists());
    }
}
