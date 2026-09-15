//! The dsh plugin's flat thread and turn protocol and browser human view.

mod approvals;
pub mod cli;
pub(super) mod rpc;
mod ui;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use super::rpc::{self as transport, Connection};
use super::{
    Backend, Event, EventContext, ModelSettings, Opened, StartedThread, Thread, Turn, WaitPolicy,
};

pub struct Dsh;

impl Backend for Dsh {
    type Run = cli::RunArgs;
    type Decision = approvals::Decision;
    const WAIT_POLICY: WaitPolicy = WaitPolicy {
        deadlines_first: true,
        isolate_selected_request: true,
        request_is_progress: false,
    };

    fn state_name(&self) -> &'static str {
        "dsh"
    }
    fn env_prefix(&self) -> &'static str {
        "AGENT_BRIDGE_DSH_"
    }

    fn daemon_command(&self, _url: &str) -> Result<std::process::Command> {
        let mut command = match std::env::var_os("AGENT_BRIDGE_DSH_BIN")
            .filter(|bin| !bin.is_empty())
        {
            Some(bin) => {
                let mut command = std::process::Command::new(node_bin());
                command.arg(bin);
                command
            }
            None => {
                #[cfg(windows)]
                let names = ["dsh.exe", "dsh.com", "dsh.cmd", "dsh.bat"];
                #[cfg(not(windows))]
                let names = ["dsh"];
                let bin = find_on_path(&names).ok_or_else(|| {
                    anyhow!("dsh was not found on PATH; set AGENT_BRIDGE_DSH_BIN to the dsh JavaScript entry point")
                })?;
                std::process::Command::new(bin)
            }
        };
        command
            .arg("--profile")
            .arg(ui::setting("AGENT_BRIDGE_DSH_PROFILE", "bridge"))
            .arg("--port")
            .arg(ui::ui_port());
        Ok(command)
    }

    async fn after_ready(&self, log_offset: u64, report: bool) -> Value {
        if report {
            json!({"uiUrl":ui::wait_for_new_ui_url(log_offset).await})
        } else {
            json!({})
        }
    }
    fn daemon_fields(&self) -> Value {
        json!({"uiUrl":ui::ui_url()})
    }

    async fn connect(&self, url: &str) -> Result<Opened> {
        let (conn, events) = transport::connect(url, &rpc::Wire).await?;
        let initialized = conn
            .request(
                "initialize",
                json!({
                    "clientInfo":{"name":"agent-bridge","version":env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
            .context("handshake failed")?;
        let ui_url = ui::logged_ui_url()
            .or_else(|| {
                initialized
                    .get("uiUrl")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(ui::ui_url);
        Ok(Opened {
            conn,
            events,
            metadata: json!({"uiUrl":ui_url}),
        })
    }

    async fn start_thread(
        &self,
        conn: &Connection,
        run: &Self::Run,
        developer_instructions: Option<&str>,
        settings: &ModelSettings,
    ) -> Result<StartedThread> {
        let opts = &run.common;
        let mut params = Map::new();
        for (key, value) in [
            ("cwd", &opts.cwd),
            ("sandbox", &opts.sandbox),
            ("approval", &run.approval),
            ("provider", &settings.provider),
            ("model", &settings.model),
            ("reasoningEffort", &settings.effort),
            ("title", &run.title),
        ] {
            if let Some(value) = value {
                params.insert(key.into(), json!(value));
            }
        }
        if let Some(text) = developer_instructions {
            params.insert("developerInstructions".into(), json!(text));
        }
        let result = conn
            .request("thread/start", Value::Object(params))
            .await
            .context("thread/start failed")?;
        started_thread(result)
    }

    async fn start_existing_thread(&self, conn: &Connection, id: &str) -> Result<StartedThread> {
        let result = conn
            .request("thread/resume", json!({"threadId":id}))
            .await
            .context("thread/resume failed")?;
        started_thread(result)
    }

    async fn resume_thread(&self, conn: &Connection, id: &str) -> Result<Thread> {
        let result = conn
            .request("thread/resume", json!({"threadId":id}))
            .await
            .context("thread/resume failed")?;
        match status_type(&result) {
            "idle" | "running" => Ok(normalize_thread(id, result)),
            status => bail!("thread {id} returned unknown status {status}"),
        }
    }

    async fn subscribe(&self, conn: &Connection, id: &str) -> Result<()> {
        conn.request("thread/resume", json!({"threadId":id}))
            .await
            .context("thread/resume failed")?;
        Ok(())
    }

    async fn read_thread(
        &self,
        conn: &Connection,
        id: &str,
        include_turns: bool,
    ) -> Result<Thread> {
        let result = conn
            .request(
                "thread/read",
                json!({"threadId":id,"includeTurns":include_turns}),
            )
            .await
            .context("thread/read failed")?;
        Ok(normalize_thread(id, result))
    }

    async fn start_turn(
        &self,
        conn: &Connection,
        id: &str,
        prompt: &str,
        _settings: &ModelSettings,
    ) -> Result<String> {
        let result = conn
            .request("turn/start", json!({"threadId":id,"text":prompt}))
            .await
            .context("turn/start failed")?;
        turn_id_of(result.get("turnId")).ok_or_else(|| anyhow!("turn/start returned no turnId"))
    }

    async fn steer(&self, conn: &Connection, id: &str, turn: &str, text: &str) -> Result<Value> {
        conn.request(
            "turn/steer",
            json!({"threadId":id,"expectedTurnId":turn_id_wire(turn),"text":text}),
        )
        .await
        .context("turn/steer failed")
    }

    async fn interrupt(&self, conn: &Connection, id: &str, turn: &str) -> Result<Value> {
        conn.request(
            "turn/interrupt",
            json!({"threadId":id,"turnId":turn_id_wire(turn)}),
        )
        .await
        .context("turn/interrupt failed")
    }

    async fn normalize(
        &self,
        _conn: &Connection,
        event: transport::Event,
        context: EventContext<'_>,
    ) -> Result<Event> {
        let main_thread = context.thread_id;
        let (method, params) = match event {
            transport::Event::Closed { reason } => return Ok(Event::Closed { reason }),
            transport::Event::ServerRequest { id, method, params } => {
                if thread_of(&params) != Some(main_thread) {
                    return Ok(Event::Ignore);
                }
                let thread_id = thread_of(&params).map(str::to_string);
                return Ok(Event::Request {
                    id,
                    method,
                    params,
                    thread_id,
                });
            }
            transport::Event::Notification { method, params } => (method, params),
        };
        if thread_of(&params) != Some(main_thread) {
            return Ok(Event::Ignore);
        }
        match method.as_str() {
            "turn/started" if context.turn_id.is_none() => Ok(Event::Update {
                turn_id: turn_id_of(params.get("turnId")),
                final_message: None,
                progress: false,
                log: None,
            }),
            "thread/event" => {
                let final_message =
                    if params.get("type").and_then(Value::as_str) == Some("assistant/message") {
                        params.get("data").and_then(event_message)
                    } else {
                        None
                    };
                Ok(Event::Update {
                    turn_id: None,
                    final_message,
                    progress: true,
                    log: Some(json!({
                        "event":"item", "threadId":main_thread,
                        "seq":params.get("seq").cloned().unwrap_or(Value::Null),
                        "itemType":params.get("type").cloned().unwrap_or(Value::Null),
                        "data":params.get("data").cloned().unwrap_or(Value::Null),
                    })),
                })
            }
            "turn/completed" => {
                if context.reply_pending {
                    bail!("turn ended before the pending server request was re-delivered");
                }
                let turn_id = turn_id_of(params.get("turnId"))
                    .ok_or_else(|| anyhow!("turn/completed returned no turnId"))?;
                if context.turn_id.is_some_and(|expected| expected != turn_id) {
                    return Ok(Event::Ignore);
                }
                let status = params
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("turn/completed returned no status"))?;
                if !matches!(status, "completed" | "failed" | "interrupted") {
                    bail!("turn/completed returned unknown status {status}");
                }
                Ok(Event::TurnCompleted {
                    turn_id: Some(turn_id),
                    status: status.to_string(),
                    final_message: params
                        .get("finalMessage")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    fields: terminal_fields(&params),
                })
            }
            "thread/status" => Ok(Event::Update {
                turn_id: None,
                final_message: None,
                progress: false,
                log: Some(json!({"event":"status","threadId":main_thread,
                    "status":params.get("status").cloned().unwrap_or(Value::Null)})),
            }),
            "request/resolved" => {
                let id = params.get("requestId").cloned().unwrap_or(Value::Null);
                Ok(Event::RequestResolved {
                    id: id.clone(),
                    log: json!({
                        "event":"requestResolved","threadId":main_thread,"requestId":id,
                        "resolvedBy":params.get("resolvedBy").cloned().unwrap_or(Value::Null),
                    }),
                })
            }
            _ => Ok(Event::Ignore),
        }
    }

    fn decision_response(&self, method: &str, decision: Self::Decision) -> Result<Value> {
        approvals::response_for(method, decision)
    }
    fn unattended(&self) -> (i64, &'static str) {
        (approvals::UNATTENDED_CODE, approvals::UNATTENDED_MESSAGE)
    }
    fn same_request_id(&self, expected: &Value, actual: &Value) -> bool {
        same_request_id(expected, actual)
    }
    async fn attach(&self, _conn: &Connection, _url: &str, _id: &str, _cwd: Option<&str>) {}
    fn started_fields(&self, started: &StartedThread, metadata: &Value) -> Value {
        let mut fields = started.fields.clone();
        fields["uiUrl"] = metadata["uiUrl"].clone();
        fields
    }
}

/// Prefer an explicit executable, then resolve PATH in directory order,
/// including Windows command shims. A resolved path lets Rust quote batch
/// arguments while ordinary node.exe processes remain direct children of the
/// daemon launcher.
fn node_bin() -> std::path::PathBuf {
    if let Some(explicit) = std::env::var_os("AGENT_BRIDGE_DSH_NODE_BIN").filter(|s| !s.is_empty())
    {
        return std::path::PathBuf::from(explicit);
    }
    #[cfg(windows)]
    if let Some(bin) = find_on_path(&["node.exe", "node.cmd", "node.bat"]) {
        return bin;
    }
    std::path::PathBuf::from("node")
}

fn find_on_path(names: &[&str]) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let Ok(path) = std::ffi::CString::new(candidate.as_os_str().as_bytes()) else {
                    continue;
                };
                // SAFETY: path is a live, null-terminated string; access only
                // checks this process's execute permission and changes no files.
                if unsafe { libc::access(path.as_ptr(), libc::X_OK) } != 0 {
                    continue;
                }
            }
            return if candidate.is_absolute() {
                Some(candidate)
            } else {
                std::env::current_dir().ok().map(|cwd| cwd.join(candidate))
            };
        }
    }
    None
}

fn started_thread(value: Value) -> Result<StartedThread> {
    let id = value
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("thread/start or thread/resume returned no threadId"))?
        .to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("thread/start or thread/resume returned no title"))?;
    let mut fields = json!({"title":title});
    for (field, wire) in [("model", "model"), ("effort", "reasoningEffort")] {
        if let Some(value) = value.get(wire).and_then(Value::as_str) {
            fields[field] = json!(value);
        }
    }
    Ok(StartedThread { id, fields })
}

fn normalize_thread(id: &str, thread: Value) -> Thread {
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Thread {
        id: id.to_string(),
        running: status_type(&thread) == "running",
        status: thread.get("status").cloned().unwrap_or(Value::Null),
        cwd: thread.get("cwd").cloned().unwrap_or(Value::Null),
        model: thread.get("model").cloned().unwrap_or(Value::Null),
        final_message: last_agent_message(&turns),
        turns: turns
            .iter()
            .map(|turn| Turn {
                id: turn_id_of(turn.get("turnId")),
                status: turn.get("status").cloned().unwrap_or(Value::Null),
                running: matches!(
                    turn.get("status").and_then(Value::as_str),
                    Some("running" | "inProgress")
                ),
                fields: terminal_fields(turn),
            })
            .collect(),
    }
}

fn terminal_fields(turn: &Value) -> Value {
    let mut fields = json!({});
    for key in ["error", "durationMs", "reason"] {
        if let Some(value) = turn.get(key).filter(|value| !value.is_null()) {
            fields[key] = value.clone();
        }
    }
    fields
}

fn status_type(thread: &Value) -> &str {
    thread
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

/// The plugin sends integer turn ids; the CLI prints their string form.
fn turn_id_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn turn_id_wire(turn_id: &str) -> Value {
    turn_id
        .parse::<i64>()
        .map_or_else(|_| Value::from(turn_id), Value::from)
}

fn last_agent_message(turns: &[Value]) -> String {
    turns
        .last()
        .and_then(|turn| turn.get("finalMessage"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn thread_of(params: &Value) -> Option<&str> {
    params.get("threadId").and_then(Value::as_str)
}
fn same_request_id(expected: &Value, actual: &Value) -> bool {
    expected.is_string() && expected == actual
}

/// Prefer the nested message used by the plugin and only collect text blocks.
fn event_message(data: &Value) -> Option<String> {
    let data = data
        .get("message")
        .filter(|message| message.is_object())
        .unwrap_or(data);
    if let Some(content) = data.get("content").and_then(Value::as_array) {
        let text: String = content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect();
        return (!text.is_empty()).then_some(text);
    }
    data.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_exact_strings() {
        assert!(same_request_id(&json!("req-7"), &json!("req-7")));
        assert!(!same_request_id(&json!("req-7"), &json!("req-8")));
        assert!(!same_request_id(&json!("7"), &json!(7)));
    }

    #[test]
    fn turn_ids_accept_the_integers_the_plugin_sends_and_go_back_as_integers() {
        assert_eq!(turn_id_of(Some(&json!(3))), Some("3".into()));
        assert_eq!(turn_id_of(Some(&json!("01a0"))), Some("01a0".into()));
        assert_eq!(turn_id_of(Some(&Value::Null)), None);
        assert_eq!(turn_id_of(None), None);
        assert_eq!(turn_id_wire("3"), json!(3));
        assert_eq!(turn_id_wire("01a0"), json!("01a0"));
    }

    #[test]
    fn thread_id_is_found_in_the_protocol_field() {
        assert_eq!(thread_of(&json!({"threadId":"a"})), Some("a"));
        assert_eq!(thread_of(&json!({})), None);
    }

    #[test]
    fn final_message_belongs_to_the_last_turn_even_when_empty() {
        let mut turns = vec![
            json!({"finalMessage":"first"}),
            json!({"finalMessage":"second"}),
        ];
        assert_eq!(last_agent_message(&turns), "second");
        turns.push(json!({"finalMessage":""}));
        assert_eq!(last_agent_message(&turns), "");
    }

    #[test]
    fn message_text_blocks_are_joined_without_empty_overwrites() {
        assert_eq!(
            event_message(
                &json!({"content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]})
            ),
            Some("ab".into())
        );
        assert_eq!(event_message(&json!({"content":[]})), None);
    }

    #[test]
    fn assistant_text_comes_from_the_nested_message_the_plugin_sends() {
        assert_eq!(
            event_message(&json!({"turn":1,"message":{"content":[
                {"type":"reasoning","text":"thinking out loud"},{"type":"text","text":"the answer"},{"type":"tool-call","text":""}
            ]}})),
            Some("the answer".into())
        );
        assert_eq!(
            event_message(&json!({"message":{"content":[{"type":"reasoning","text":"x"}]}})),
            None
        );
    }
}
