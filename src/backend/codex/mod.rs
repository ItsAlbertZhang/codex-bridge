//! Codex app-server operations, event normalization and the Herdr human view.

mod approvals;
pub mod cli;
mod pane;
mod rpc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use super::rpc::{self as transport, Connection};
use super::{
    Backend, Event, EventContext, ModelSettings, Opened, StartedThread, Thread, Turn, WaitPolicy,
};

pub struct Codex {
    pub no_pane: bool,
}

impl Backend for Codex {
    type Run = cli::RunArgs;
    type Decision = approvals::Decision;
    const WAIT_POLICY: WaitPolicy = WaitPolicy {
        deadlines_first: false,
        isolate_selected_request: false,
        request_is_progress: true,
    };

    fn state_name(&self) -> &'static str {
        "codex"
    }
    fn env_prefix(&self) -> &'static str {
        "AGENT_BRIDGE_CODEX_"
    }

    fn daemon_command(&self, url: &str) -> Result<std::process::Command> {
        let bin = std::env::var("AGENT_BRIDGE_CODEX_BIN")
            .ok()
            .filter(|bin| !bin.is_empty())
            .unwrap_or_else(|| "codex".to_string());
        let mut command = std::process::Command::new(bin);
        command.args(["app-server", "--listen", url]);
        Ok(command)
    }

    async fn after_ready(&self, _log_offset: u64, _report: bool) -> Value {
        json!({})
    }
    fn daemon_fields(&self) -> Value {
        json!({})
    }

    async fn connect(&self, url: &str) -> Result<Opened> {
        let (conn, events) = transport::connect(url, &rpc::Wire).await?;
        let metadata = conn
            .request(
                "initialize",
                json!({
                    "clientInfo":{"name":"agent-bridge","version":env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
            .context("handshake failed")?;
        conn.notify("initialized", Value::Null)
            .await
            .context("handshake failed")?;
        Ok(Opened {
            conn,
            events,
            metadata,
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
            ("approvalPolicy", &run.approval),
            ("model", &settings.model),
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
        let id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread/start returned no thread.id"))?
            .to_string();
        Ok(StartedThread {
            id,
            fields: model_fields(&result),
        })
    }

    async fn start_existing_thread(&self, conn: &Connection, id: &str) -> Result<StartedThread> {
        let result = conn
            .request("thread/resume", json!({"threadId":id}))
            .await
            .context("thread/resume failed")?;
        Ok(StartedThread {
            id: id.to_string(),
            fields: model_fields(&result),
        })
    }

    async fn resume_thread(&self, conn: &Connection, id: &str) -> Result<Thread> {
        let result = conn
            .request("thread/resume", json!({"threadId":id}))
            .await
            .context("thread/resume failed")?;
        let thread = result.get("thread").cloned().unwrap_or(result);
        if status_type(&thread) == "systemError" {
            bail!("thread {id} is in systemError");
        }
        Ok(normalize_thread(id, thread))
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
        let thread = result.get("thread").cloned().unwrap_or(result);
        Ok(normalize_thread(id, thread))
    }

    async fn start_turn(
        &self,
        conn: &Connection,
        id: &str,
        prompt: &str,
        settings: &ModelSettings,
    ) -> Result<String> {
        let mut params = json!({"threadId":id,"input":[{"type":"text","text":prompt}]});
        if let Some(effort) = &settings.effort {
            params["effort"] = json!(effort);
        }
        let result = conn
            .request("turn/start", params)
            .await
            .context("turn/start failed")?;
        result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("turn/start returned no turn.id"))
    }

    async fn steer(&self, conn: &Connection, id: &str, turn: &str, text: &str) -> Result<Value> {
        conn.request(
            "turn/steer",
            json!({"threadId":id,"expectedTurnId":turn,"input":[{"type":"text","text":text}]}),
        )
        .await
        .context("turn/steer failed")
    }

    async fn interrupt(&self, conn: &Connection, id: &str, turn: &str) -> Result<Value> {
        conn.request("turn/interrupt", json!({"threadId":id,"turnId":turn}))
            .await
            .context("turn/interrupt failed")
    }

    async fn normalize(
        &self,
        conn: &Connection,
        event: transport::Event,
        context: EventContext<'_>,
    ) -> Result<Event> {
        let main_thread = context.thread_id;
        let (method, params) = match event {
            transport::Event::Closed { reason } => return Ok(Event::Closed { reason }),
            transport::Event::ServerRequest { id, method, params } => {
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
        if thread_of(&params).is_some_and(|thread| thread != main_thread) {
            return Ok(Event::Ignore);
        }
        let mut update = Event::Update {
            turn_id: None,
            final_message: None,
            progress: false,
            log: None,
        };
        match method.as_str() {
            "turn/started" => {
                update = Event::Update {
                    turn_id: params
                        .pointer("/turn/id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    final_message: None,
                    progress: true,
                    log: None,
                };
            }
            "item/completed" => {
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                let final_message = match item.get("type").and_then(Value::as_str) {
                    Some("agentMessage") => {
                        item.get("text").and_then(Value::as_str).map(str::to_string)
                    }
                    Some("subAgentActivity")
                        if item.get("kind").and_then(Value::as_str) == Some("started") =>
                    {
                        if let Some(sub) = item.get("agentThreadId").and_then(Value::as_str) {
                            conn.request_no_reply("thread/unsubscribe", json!({"threadId":sub}))
                                .await?;
                        }
                        None
                    }
                    _ => None,
                };
                update = Event::Update {
                    turn_id: None,
                    final_message,
                    progress: true,
                    log: Some(item_summary(main_thread, &params, &item)),
                };
            }
            "turn/completed" => {
                let turn = params.get("turn").cloned().unwrap_or(Value::Null);
                let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_string);
                let status = turn
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_string();
                if status == "inProgress" {
                    return Ok(Event::Update {
                        turn_id,
                        final_message: None,
                        progress: false,
                        log: None,
                    });
                }
                return Ok(Event::TurnCompleted {
                    turn_id,
                    status,
                    final_message: None,
                    fields: terminal_fields(&turn),
                });
            }
            "thread/status/changed" => {
                update = log_update(json!({"event":"status","threadId":main_thread,
                    "status":params.get("status").cloned().unwrap_or(Value::Null)}));
            }
            "serverRequest/resolved" => {
                update = log_update(json!({"event":"requestResolved","threadId":main_thread,
                    "requestId":params.get("requestId").cloned().unwrap_or(Value::Null)}));
            }
            "error" => {
                update = log_update(
                    json!({"event":"serverError","threadId":main_thread,"params":params}),
                )
            }
            _ => {}
        }
        Ok(update)
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
    async fn attach(&self, conn: &Connection, url: &str, id: &str, cwd: Option<&str>) {
        let Some(bin) = pane::herdr_bin(self.no_pane) else {
            return;
        };
        let cwd = match cwd {
            Some(cwd) => cwd.to_string(),
            None => self
                .read_thread(conn, id, false)
                .await
                .ok()
                .and_then(|thread| thread.cwd.as_str().map(str::to_string))
                .unwrap_or_default(),
        };
        if let Err(err) = pane::open(&bin, id, &cwd, url).await {
            eprintln!("agent-bridge codex: herdr pane: {err:#}");
        }
    }
    fn started_fields(&self, started: &StartedThread, _metadata: &Value) -> Value {
        started.fields.clone()
    }
}

/// Only server-reported thread metadata belongs in started; never guess from
/// the requested settings. Some server versions return these beside thread.
fn model_fields(result: &Value) -> Value {
    let mut fields = json!({});
    for (field, wire) in [("model", "model"), ("effort", "reasoningEffort")] {
        if let Some(value) = result
            .get("thread")
            .and_then(|thread| thread.get(wire))
            .and_then(Value::as_str)
            .or_else(|| result.get(wire).and_then(Value::as_str))
        {
            fields[field] = json!(value);
        }
    }
    fields
}

fn log_update(log: Value) -> Event {
    Event::Update {
        turn_id: None,
        final_message: None,
        progress: false,
        log: Some(log),
    }
}

fn normalize_thread(id: &str, thread: Value) -> Thread {
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Thread {
        id: id.to_string(),
        running: status_type(&thread) == "active",
        status: thread.get("status").cloned().unwrap_or(Value::Null),
        cwd: thread.get("cwd").cloned().unwrap_or(Value::Null),
        model: thread.get("model").cloned().unwrap_or(Value::Null),
        final_message: last_agent_message(&turns),
        turns: turns
            .iter()
            .map(|turn| Turn {
                id: turn.get("id").and_then(Value::as_str).map(str::to_string),
                status: turn.get("status").cloned().unwrap_or(Value::Null),
                running: turn.get("status").and_then(Value::as_str) == Some("inProgress"),
                fields: terminal_fields(turn),
            })
            .collect(),
    }
}

fn terminal_fields(turn: &Value) -> Value {
    let mut fields = json!({});
    for key in ["error", "durationMs"] {
        if let Some(value) = turn.get(key).filter(|value| !value.is_null()) {
            fields[key] = value.clone();
        }
    }
    fields
}

fn status_type(thread: &Value) -> &str {
    thread
        .pointer("/status/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn last_agent_message(turns: &[Value]) -> String {
    let mut text = String::new();
    for turn in turns {
        if let Some(items) = turn.get("items").and_then(Value::as_array) {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    if let Some(value) = item.get("text").and_then(Value::as_str) {
                        text = value.to_string();
                    }
                }
            }
        }
    }
    text
}

fn thread_of(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/thread/id").and_then(Value::as_str))
        .or_else(|| params.pointer("/item/threadId").and_then(Value::as_str))
}

fn same_request_id(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    match (expected.as_i64(), actual.as_str()) {
        (Some(n), Some(s)) => s == n.to_string(),
        _ => match (actual.as_i64(), expected.as_str()) {
            (Some(n), Some(s)) => s == n.to_string(),
            _ => false,
        },
    }
}

fn item_summary(thread_id: &str, params: &Value, item: &Value) -> Value {
    let mut summary = json!({"event":"item","threadId":thread_id,
        "turnId":params.get("turnId").cloned().unwrap_or(Value::Null),
        "itemType":item.get("type").and_then(Value::as_str).unwrap_or("unknown")});
    for key in [
        "text",
        "command",
        "exitCode",
        "status",
        "kind",
        "agentThreadId",
        "changes",
    ] {
        if let Some(value) = item.get(key) {
            summary[key] = value.clone();
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_compare_across_json_types() {
        assert!(same_request_id(&json!(7), &json!(7)));
        assert!(same_request_id(&json!(7), &json!("7")));
        assert!(same_request_id(&json!("7"), &json!(7)));
        assert!(!same_request_id(&json!(7), &json!(8)));
    }

    #[test]
    fn thread_id_is_found_in_every_shape() {
        assert_eq!(thread_of(&json!({"threadId":"a"})), Some("a"));
        assert_eq!(thread_of(&json!({"thread":{"id":"b"}})), Some("b"));
        assert_eq!(thread_of(&json!({"item":{"threadId":"c"}})), Some("c"));
        assert_eq!(thread_of(&json!({})), None);
    }

    #[test]
    fn final_message_is_the_last_agent_message() {
        let turns = vec![
            json!({"items":[{"type":"agentMessage","text":"first"}]}),
            json!({"items":[{"type":"commandExecution"},{"type":"agentMessage","text":"second"}]}),
        ];
        assert_eq!(last_agent_message(&turns), "second");
    }
}
