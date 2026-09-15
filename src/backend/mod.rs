//! Backend boundaries: wire protocols become the events consumed by the core.

pub mod codex;
pub mod dsh;
pub mod rpc;

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

pub trait RunConfig {
    fn common(&self) -> &crate::cli::RunArgs;
}

pub struct Opened {
    pub conn: Arc<rpc::Connection>,
    pub events: UnboundedReceiver<rpc::Event>,
    pub metadata: Value,
}

pub struct StartedThread {
    pub id: String,
    pub fields: Value,
}

/// Final input settings after CLI/config precedence; missing values stay omitted.
#[derive(Clone, Default)]
pub struct ModelSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub provider: Option<String>,
}

pub struct Thread {
    pub id: String,
    pub running: bool,
    pub status: Value,
    pub cwd: Value,
    pub model: Value,
    pub turns: Vec<Turn>,
    pub final_message: String,
}

pub struct Turn {
    pub id: Option<String>,
    pub status: Value,
    pub running: bool,
    /// Backend-specific terminal fields, with absent and null values omitted.
    pub fields: Value,
}

/// Preserve the observable scheduling and redelivery contracts of each server.
pub struct WaitPolicy {
    pub deadlines_first: bool,
    pub isolate_selected_request: bool,
    pub request_is_progress: bool,
}

pub struct EventContext<'a> {
    pub thread_id: &'a str,
    pub turn_id: Option<&'a str>,
    pub reply_pending: bool,
}

pub enum Event {
    Ignore,
    Closed {
        reason: String,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
        thread_id: Option<String>,
    },
    Update {
        turn_id: Option<String>,
        final_message: Option<String>,
        progress: bool,
        log: Option<Value>,
    },
    TurnCompleted {
        turn_id: Option<String>,
        status: String,
        final_message: Option<String>,
        fields: Value,
    },
    RequestResolved {
        id: Value,
        log: Value,
    },
}

/// The core owns lifecycle and output; backends own every wire-level choice.
pub trait Backend {
    type Run: RunConfig;
    type Decision: Copy;
    const WAIT_POLICY: WaitPolicy;

    fn state_name(&self) -> &'static str;
    fn env_prefix(&self) -> &'static str;
    fn daemon_command(&self, url: &str) -> Result<std::process::Command>;
    async fn after_ready(&self, log_offset: u64, report: bool) -> Value;
    fn daemon_fields(&self) -> Value;

    async fn connect(&self, url: &str) -> Result<Opened>;
    async fn start_thread(
        &self,
        conn: &rpc::Connection,
        run: &Self::Run,
        developer_instructions: Option<&str>,
        settings: &ModelSettings,
    ) -> Result<StartedThread>;
    async fn start_existing_thread(
        &self,
        conn: &rpc::Connection,
        id: &str,
    ) -> Result<StartedThread>;
    async fn resume_thread(&self, conn: &rpc::Connection, id: &str) -> Result<Thread>;
    /// Bind the thread without interpreting its resume result during selection.
    async fn subscribe(&self, conn: &rpc::Connection, id: &str) -> Result<()>;
    async fn read_thread(
        &self,
        conn: &rpc::Connection,
        id: &str,
        include_turns: bool,
    ) -> Result<Thread>;
    async fn start_turn(
        &self,
        conn: &rpc::Connection,
        id: &str,
        prompt: &str,
        settings: &ModelSettings,
    ) -> Result<String>;
    async fn steer(
        &self,
        conn: &rpc::Connection,
        id: &str,
        turn: &str,
        text: &str,
    ) -> Result<Value>;
    async fn interrupt(&self, conn: &rpc::Connection, id: &str, turn: &str) -> Result<Value>;
    async fn normalize(
        &self,
        conn: &rpc::Connection,
        event: rpc::Event,
        context: EventContext<'_>,
    ) -> Result<Event>;
    /// Completion parsing is deferred until the implicit selection window ends.
    fn defer_during_selection(&self, event: &rpc::Event) -> bool {
        matches!(event, rpc::Event::Notification { method, .. } if method == "turn/completed")
    }

    fn decision_response(&self, method: &str, decision: Self::Decision) -> Result<Value>;
    fn unattended(&self) -> (i64, &'static str);
    fn same_request_id(&self, expected: &Value, actual: &Value) -> bool;
    async fn attach(&self, conn: &rpc::Connection, url: &str, id: &str, cwd: Option<&str>);
    fn started_fields(&self, started: &StartedThread, metadata: &Value) -> Value;
}
