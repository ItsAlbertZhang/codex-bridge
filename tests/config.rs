//! Configuration paths and precedence use only scratch files and mock backends.
//! Codex represents shared loading behavior; both adapters verify their wire fields.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use common::{code, lines, ok, start_mock, Recorder, TempDir};
use serde_json::{json, Value};

const THREAD: &str = "T-config";
const DEFAULTS: &str = "[codex]\nmodel = 'mock-config-codex'\neffort = 'low'\n\
                       [dsh]\nprovider = 'mock-provider'\nmodel = 'mock-config-dsh'\neffort = 'low'\n";

fn write_config(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("config directory");
    std::fs::write(path, text).expect("config file");
}

fn server(backend: &'static str, start_response: Option<Value>) -> (u16, Recorder) {
    let recorder = Recorder::default();
    let seen = recorder.clone();
    let created = start_response.unwrap_or_else(|| match backend {
        "codex" => json!({"thread": {"id": THREAD}}),
        "dsh" => json!({"threadId": THREAD, "title": "mock title"}),
        _ => unreachable!(),
    });
    let port = start_mock(Arc::new(move |msg, tx| {
        seen.push(msg.clone());
        match msg["method"].as_str().unwrap_or("") {
            "initialize" => ok(tx, msg, json!({})),
            "thread/start" | "thread/resume" => ok(tx, msg, created.clone()),
            "thread/read" => {
                let thread = match backend {
                    "codex" => json!({"thread": {
                        "id": THREAD, "status": {"type": "idle"},
                        "cwd": env!("CARGO_MANIFEST_DIR"), "model": "mock-read-model", "turns": []
                    }}),
                    "dsh" => json!({
                        "threadId": THREAD, "status": "idle", "title": "mock title",
                        "cwd": env!("CARGO_MANIFEST_DIR"), "model": "mock-read-model", "turns": []
                    }),
                    _ => unreachable!(),
                };
                ok(tx, msg, thread);
            }
            "turn/start" => ok(
                tx,
                msg,
                match backend {
                    "codex" => json!({"turn": {"id": "U-config"}}),
                    "dsh" => json!({"turnId": 7}),
                    _ => unreachable!(),
                },
            ),
            _ => {}
        }
    }));
    (port, recorder)
}

fn cli(backend: &str, port: u16, state: &TempDir) -> Command {
    match backend {
        "codex" => common::cli(port, state),
        "dsh" => common::dsh::cli(port, state),
        _ => unreachable!(),
    }
}

fn run(backend: &str, port: u16, state: &TempDir) -> Command {
    let mut command = cli(backend, port, state);
    command
        .args(["run", "--cwd"])
        .arg(state.path())
        .args(["--prompt", "hi", "--no-wait"]);
    command
}

fn start_params(recorder: &Recorder) -> Value {
    let requests = recorder.requests("thread/start");
    assert_eq!(requests.len(), 1);
    requests[0]["params"].clone()
}

fn effort_param(backend: &str, recorder: &Recorder) -> Option<Value> {
    match backend {
        "codex" => recorder.requests("turn/start")[0]["params"]
            .get("effort")
            .cloned(),
        "dsh" => start_params(recorder).get("reasoningEffort").cloned(),
        _ => unreachable!(),
    }
}

#[test]
fn explicit_config_path_takes_precedence_over_xdg_and_home() {
    let state = TempDir::new("config-explicit");
    let explicit = state.join("selected config.toml");
    let xdg = state.join("xdg-data");
    let home = state.join("mock-home");
    write_config(&explicit, "[codex]\nmodel = 'mock-explicit'\n");
    write_config(
        &xdg.join("agent-bridge/config.toml"),
        "[codex]\nmodel = 'mock-xdg'\n",
    );
    write_config(
        &home.join(".local/share/agent-bridge/config.toml"),
        "[codex]\nmodel = 'mock-home'\n",
    );
    let (port, recorder) = server("codex", None);
    let output = run("codex", port, &state)
        .env("AGENT_BRIDGE_CONFIG", &explicit)
        .env("XDG_DATA_HOME", &xdg)
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(start_params(&recorder)["model"], "mock-explicit");
}

#[test]
fn xdg_config_path_takes_precedence_over_home() {
    let state = TempDir::new("config-xdg");
    let xdg = state.join("xdg-data");
    let home = state.join("mock-home");
    write_config(
        &xdg.join("agent-bridge/config.toml"),
        "[codex]\nmodel = 'mock-xdg'\n",
    );
    write_config(
        &home.join(".local/share/agent-bridge/config.toml"),
        "[codex]\nmodel = 'mock-home'\n",
    );
    let (port, recorder) = server("codex", None);
    let output = run("codex", port, &state)
        .env_remove("AGENT_BRIDGE_CONFIG")
        .env("XDG_DATA_HOME", &xdg)
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(start_params(&recorder)["model"], "mock-xdg");
}

#[test]
fn the_platform_home_supplies_the_default_config_path() {
    let state = TempDir::new("config-home");
    let home = state.join("mock-home");
    let other = state.join("other-platform-home");
    write_config(
        &home.join(".local/share/agent-bridge/config.toml"),
        "[codex]\nmodel = 'mock-home'\n",
    );
    write_config(
        &other.join(".local/share/agent-bridge/config.toml"),
        "[codex]\nmodel = 'mock-other-home'\n",
    );
    let (port, recorder) = server("codex", None);
    let output = run("codex", port, &state)
        .env_remove("AGENT_BRIDGE_CONFIG")
        .env_remove("XDG_DATA_HOME")
        .env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }, &home)
        .env(if cfg!(windows) { "HOME" } else { "USERPROFILE" }, &other)
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(start_params(&recorder)["model"], "mock-home");
}

#[test]
fn cli_model_and_effort_override_config_defaults_for_each_backend() {
    for backend in ["codex", "dsh"] {
        for (model, effort) in [
            (Some("mock-cli-model"), Some("high")),
            (Some("mock-cli-model"), None),
            (None, Some("high")),
        ] {
            let state = TempDir::new("config-cli-precedence");
            let config = state.join("config.toml");
            write_config(&config, DEFAULTS);
            let (port, recorder) = server(backend, None);
            let mut command = run(backend, port, &state);
            command.env("AGENT_BRIDGE_CONFIG", &config);
            if let Some(model) = model {
                command.args(["--model", model]);
            }
            if let Some(effort) = effort {
                command.args(["--effort", effort]);
            }
            let output = command.output().unwrap();
            assert_eq!(code(&output), 0, "{backend}: {output:?}");
            let expected_model = model
                .map(str::to_string)
                .unwrap_or_else(|| format!("mock-config-{backend}"));
            assert_eq!(start_params(&recorder)["model"], expected_model);
            assert_eq!(
                effort_param(backend, &recorder),
                Some(json!(effort.unwrap_or("low")))
            );
        }
    }
}

#[test]
fn config_supplies_model_and_effort_when_cli_options_are_absent() {
    for backend in ["codex", "dsh"] {
        let state = TempDir::new("config-defaults");
        let config = state.join("config.toml");
        write_config(&config, DEFAULTS);
        let (port, recorder) = server(backend, None);
        let output = run(backend, port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{backend}: {output:?}");
        assert_eq!(
            start_params(&recorder)["model"],
            format!("mock-config-{backend}")
        );
        assert_eq!(effort_param(backend, &recorder), Some(json!("low")));
    }
}

#[test]
fn a_missing_selected_config_omits_defaults_without_trying_other_paths() {
    for backend in ["codex", "dsh"] {
        let state = TempDir::new("config-missing");
        let xdg = state.join("xdg-data");
        write_config(&xdg.join("agent-bridge/config.toml"), DEFAULTS);
        let (port, recorder) = server(backend, None);
        let output = run(backend, port, &state)
            .env("XDG_DATA_HOME", &xdg)
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{backend}: {output:?}");
        let params = start_params(&recorder);
        assert!(params.get("model").is_none());
        assert!(params.get("provider").is_none());
        assert!(effort_param(backend, &recorder).is_none());
    }
}

#[test]
fn invalid_config_reports_its_path_before_any_network_or_spawn() {
    for invalid in ["[codex\nmodel = 'broken'\n", "[codex]\nmodel = 7\n"] {
        let state = TempDir::new("config-invalid");
        let config = state.join("invalid config.toml");
        write_config(&config, invalid);
        let (port, recorder) = server("codex", None);
        let output = run("codex", port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(code(&output), 4, "{output:?}");
        let events = lines(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "error");
        assert!(events[0]["message"]
            .as_str()
            .unwrap()
            .contains(&config.display().to_string()));
        assert!(recorder.all().is_empty());
        assert!(!state.join("codex-args.txt").exists());
    }
}

#[test]
fn unknown_config_keys_report_the_path_and_key_before_connecting() {
    for (invalid, key) in [
        ("unknown = true\n", "unknown"),
        ("[codex]\nprovider = 'mock-provider'\n", "provider"),
        ("[dsh]\neffrot = 'low'\n", "effrot"),
    ] {
        let state = TempDir::new("config-unknown");
        let config = state.join("unknown config.toml");
        write_config(&config, invalid);
        let (port, recorder) = server("codex", None);
        let output = run("codex", port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(code(&output), 4, "{output:?}");
        let events = lines(&output);
        let message = events[0]["message"].as_str().unwrap();
        assert_eq!(events[0]["event"], "error");
        assert!(message.contains(&config.display().to_string()), "{message}");
        assert!(message.contains(key), "{message}");
        assert!(recorder.all().is_empty());
        assert!(!state.join("codex-args.txt").exists());
    }
}

// Codex-specific: prefer actual nested thread fields, with same-response top-level fallback.
#[test]
fn codex_started_reports_actual_response_fields_and_omits_missing_fields() {
    for (response, model, effort) in [
        (
            json!({"thread": {"id": THREAD, "model": "mock-nested", "reasoningEffort": "high"},
            "model": "mock-outer", "reasoningEffort": "low"}),
            Some("mock-nested"),
            Some("high"),
        ),
        (
            json!({"thread": {"id": THREAD}, "model": "mock-top", "reasoningEffort": "medium"}),
            Some("mock-top"),
            Some("medium"),
        ),
        (
            json!({"thread": {"id": THREAD, "model": "mock-model-only"}}),
            Some("mock-model-only"),
            None,
        ),
        (json!({"thread": {"id": THREAD}}), None, None),
    ] {
        let state = TempDir::new("config-started-codex");
        let config = state.join("config.toml");
        write_config(&config, DEFAULTS);
        let (port, recorder) = server("codex", Some(response));
        let output = run("codex", port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{output:?}");
        assert_eq!(start_params(&recorder)["model"], "mock-config-codex");
        let events = lines(&output);
        assert_eq!(events[0]["event"], "started");
        assert_eq!(events[0].get("model"), model.map(Value::from).as_ref());
        assert_eq!(events[0].get("effort"), effort.map(Value::from).as_ref());
        assert!(events[0].get("reasoningEffort").is_none());
    }
}

#[test]
fn dsh_started_reports_actual_response_fields_and_omits_missing_fields() {
    for (fields, model, effort) in [
        (
            json!({"model": "mock-actual", "reasoningEffort": "high"}),
            Some("mock-actual"),
            Some("high"),
        ),
        (
            json!({"model": "mock-model-only"}),
            Some("mock-model-only"),
            None,
        ),
        (json!({}), None, None),
    ] {
        let state = TempDir::new("config-started-dsh");
        let config = state.join("config.toml");
        write_config(&config, DEFAULTS);
        let mut response = json!({"threadId": THREAD, "title": "mock title"});
        response
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let (port, recorder) = server("dsh", Some(response));
        let output = run("dsh", port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{output:?}");
        assert_eq!(start_params(&recorder)["model"], "mock-config-dsh");
        let events = lines(&output);
        assert_eq!(events[0]["event"], "started");
        assert_eq!(events[0].get("model"), model.map(Value::from).as_ref());
        assert_eq!(events[0].get("effort"), effort.map(Value::from).as_ref());
        assert!(events[0].get("reasoningEffort").is_none());
    }
}

#[test]
fn provider_comes_only_from_dsh_config_and_has_no_cli_flag() {
    for (backend, model) in [
        ("codex", Some("mock-cli-model")),
        ("dsh", Some("mock-cli-model")),
        ("dsh", None),
    ] {
        let state = TempDir::new("config-provider");
        let config = state.join("config.toml");
        write_config(
            &config,
            "[codex]\nmodel = 'mock-codex'\n[dsh]\nprovider = 'mock-provider'\n",
        );
        let (port, recorder) = server(backend, None);
        let mut command = run(backend, port, &state);
        command.env("AGENT_BRIDGE_CONFIG", &config);
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        let output = command.output().unwrap();
        assert_eq!(code(&output), 0, "{backend}: {output:?}");
        let params = start_params(&recorder);
        assert_eq!(params.get("model"), model.map(Value::from).as_ref());
        assert_eq!(
            params.get("provider"),
            (backend == "dsh").then(|| json!("mock-provider")).as_ref()
        );
        assert!(effort_param(backend, &recorder).is_none());
        let requests_before = recorder.all().len();
        let rejected = run(backend, port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .args(["--provider", "mock-cli-provider"])
            .output()
            .unwrap();
        assert_eq!(code(&rejected), 4, "{rejected:?}");
        assert!(rejected.stdout.is_empty());
        assert_eq!(recorder.all().len(), requests_before);
    }
}

#[test]
fn existing_threads_ignore_config_defaults_and_still_reject_creation_flags() {
    for backend in ["codex", "dsh"] {
        let state = TempDir::new("config-existing-thread");
        let config = state.join("config.toml");
        write_config(&config, DEFAULTS);
        let (port, recorder) = server(backend, None);
        let output = cli(backend, port, &state)
            .env("AGENT_BRIDGE_CONFIG", &config)
            .args(["run", "--thread", THREAD, "--prompt", "hi", "--no-wait"])
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{backend}: {output:?}");
        assert!(recorder.requests("thread/start").is_empty());
        assert_eq!(
            recorder.requests("thread/resume")[0]["params"],
            json!({"threadId": THREAD})
        );
        let turn = recorder.requests("turn/start")[0]["params"].clone();
        for field in ["model", "provider", "effort", "reasoningEffort"] {
            assert!(turn.get(field).is_none(), "{backend}: {turn}");
        }
        let requests_before = recorder.all().len();
        for (flag, value) in [("--model", "mock-cli-model"), ("--effort", "high")] {
            let rejected = cli(backend, port, &state)
                .env("AGENT_BRIDGE_CONFIG", &config)
                .args(["run", "--thread", THREAD, "--prompt", "hi", flag, value])
                .output()
                .unwrap();
            assert_eq!(code(&rejected), 4, "{backend}: {rejected:?}");
            assert!(rejected.stdout.is_empty());
        }
        assert_eq!(recorder.all().len(), requests_before);
    }
}
