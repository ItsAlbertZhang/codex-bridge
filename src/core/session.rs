//! Shared session control: wait deadlines, request selection, follow and exits.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use super::{daemon, extend_fields, logging::Emitter};
use crate::backend::{rpc, Backend, Event, EventContext, ModelSettings, Opened, RunConfig, Thread};
use crate::cli::{Global, WaitFlags};

const REPLY_REDELIVERY_SECS: u64 = 10;
const NEVER: Duration = Duration::from_secs(365 * 24 * 3600);

pub const EXIT_COMPLETED: i32 = 0;
pub const EXIT_TURN_NOT_COMPLETED: i32 = 1;
pub const EXIT_REQUEST_PENDING: i32 = 2;
pub const EXIT_STALLED: i32 = 3;
pub const EXIT_ERROR: i32 = 4;

pub enum ReplyPayload<D> {
    Decision(D),
    ResultJson(Value),
}

struct PendingReply<D> {
    id: Value,
    payload: ReplyPayload<D>,
    deadline: Instant,
}

struct Session<'a, B: Backend> {
    backend: &'a B,
    conn: Arc<rpc::Connection>,
    events: UnboundedReceiver<rpc::Event>,
    thread_id: String,
    turn_id: Option<String>,
    final_message: String,
    out: Emitter,
}

enum Step {
    Continue,
    Exit(i32),
}

async fn open(
    backend: &impl Backend,
    global: &Global,
    thread_id: Option<&str>,
) -> Result<(Opened, Emitter)> {
    let out = if global.no_log {
        Emitter::new(None)?
    } else if let Some(path) = &global.log {
        Emitter::new(Some(path))?
    } else if let Some(id) = thread_id {
        Emitter::for_thread(backend, id)?
    } else {
        Emitter::new(None)?
    };
    daemon::ensure(backend, &global.url).await?;
    Ok((backend.connect(&global.url).await?, out))
}

pub async fn run<B: Backend>(
    backend: &B,
    global: &Global,
    args: &B::Run,
    prompt: &str,
    developer_instructions: Option<&str>,
    settings: &ModelSettings,
) -> Result<i32> {
    let common = args.common();
    let (opened, mut out) = open(backend, global, common.thread.as_deref()).await?;
    let started = match &common.thread {
        Some(id) => backend.start_existing_thread(&opened.conn, id).await?,
        None => {
            backend
                .start_thread(&opened.conn, args, developer_instructions, settings)
                .await?
        }
    };
    if !global.no_log && global.log.is_none() && common.thread.is_none() {
        // Queued events predate knowing the new id; never backfill its log.
        let unlogged_events = opened.events.len();
        out = Emitter::for_thread(backend, &started.id)?;
        out.unlogged_events = unlogged_events;
    }
    let fields = backend.started_fields(&started, &opened.metadata);
    let mut session = Session::new(backend, opened, started.id, out);
    let thread = session.read_thread(false).await?;
    if thread.running {
        bail!(
            "thread {} is already running ({}); refusing to start another turn",
            thread.id,
            thread.status
        );
    }
    session.turn_id = Some(
        backend
            .start_turn(&session.conn, &session.thread_id, prompt, settings)
            .await?,
    );
    let mut event =
        json!({"event":"started", "threadId":session.thread_id, "turnId":session.turn_id});
    extend_fields(&mut event, fields);
    session.out.emit(&event);
    backend
        .attach(
            &session.conn,
            &global.url,
            &session.thread_id,
            common.cwd.as_deref().or_else(|| thread.cwd.as_str()),
        )
        .await;
    if common.no_wait {
        return Ok(EXIT_COMPLETED);
    }
    session.wait_loop(&common.wait, None).await
}

pub async fn wait<B: Backend>(
    backend: &B,
    global: &Global,
    thread_id: &str,
    opts: &WaitFlags,
) -> Result<i32> {
    let (opened, out) = open(backend, global, Some(thread_id)).await?;
    let mut session = Session::new(backend, opened, thread_id.to_string(), out);
    backend
        .attach(&session.conn, &global.url, thread_id, None)
        .await;
    if let Some(code) = session.resume().await? {
        return Ok(code);
    }
    session.wait_loop(opts, None).await
}

pub async fn reply<B: Backend>(
    backend: &B,
    global: &Global,
    thread_id: &str,
    request_id: Option<Value>,
    payload: ReplyPayload<B::Decision>,
    opts: &WaitFlags,
) -> Result<i32> {
    let (opened, out) = open(backend, global, Some(thread_id)).await?;
    let mut session = Session::new(backend, opened, thread_id.to_string(), out);
    backend
        .attach(&session.conn, &global.url, thread_id, None)
        .await;
    let Some(id) = request_id else {
        return session.reply_to_only_request(payload, opts).await;
    };
    if let Some(code) = session.resume().await? {
        return Ok(code);
    }
    let pending = PendingReply {
        id,
        payload,
        deadline: Instant::now() + Duration::from_secs(REPLY_REDELIVERY_SECS),
    };
    session.wait_loop(opts, Some(pending)).await
}

pub async fn steer<B: Backend>(
    backend: &B,
    global: &Global,
    thread_id: &str,
    turn: Option<&str>,
    text: &str,
) -> Result<i32> {
    let (opened, mut out) = open(backend, global, Some(thread_id)).await?;
    backend
        .attach(&opened.conn, &global.url, thread_id, None)
        .await;
    let turn_id = match turn {
        Some(id) => id.to_string(),
        None => active_turn(
            &backend.read_thread(&opened.conn, thread_id, true).await?,
            thread_id,
            "steer",
        )?,
    };
    let result = backend
        .steer(&opened.conn, thread_id, &turn_id, text)
        .await?;
    out.emit(&json!({"event":"steered", "threadId":thread_id, "result":result}));
    Ok(EXIT_COMPLETED)
}

pub async fn interrupt(backend: &impl Backend, global: &Global, thread_id: &str) -> Result<i32> {
    let (opened, mut out) = open(backend, global, Some(thread_id)).await?;
    let thread = backend.read_thread(&opened.conn, thread_id, true).await?;
    let turn_id = active_turn(&thread, thread_id, "interrupt")?;
    let result = backend.interrupt(&opened.conn, thread_id, &turn_id).await?;
    out.emit(
        &json!({"event":"interrupted", "threadId":thread_id, "turnId":turn_id, "result":result}),
    );
    Ok(EXIT_COMPLETED)
}

pub async fn status(backend: &impl Backend, global: &Global, thread_id: &str) -> Result<i32> {
    let (opened, mut out) = open(backend, global, Some(thread_id)).await?;
    let thread = backend.read_thread(&opened.conn, thread_id, false).await?;
    out.emit(&json!({"threadId":thread_id, "status":thread.status, "cwd":thread.cwd, "model":thread.model}));
    Ok(EXIT_COMPLETED)
}

pub async fn read(backend: &impl Backend, global: &Global, thread_id: &str) -> Result<i32> {
    let (opened, mut out) = open(backend, global, Some(thread_id)).await?;
    let thread = backend.read_thread(&opened.conn, thread_id, true).await?;
    let turns: Vec<Value> = thread
        .turns
        .iter()
        .map(|t| json!({"id":t.id, "status":t.status}))
        .collect();
    out.emit(&json!({"threadId":thread_id, "turns":turns, "finalMessage":thread.final_message}));
    Ok(EXIT_COMPLETED)
}

fn active_turn(thread: &Thread, thread_id: &str, action: &str) -> Result<String> {
    thread
        .turns
        .iter()
        .rev()
        .find(|turn| turn.running)
        .and_then(|turn| turn.id.clone())
        .ok_or_else(|| anyhow!("thread {thread_id} has no turn in progress to {action}"))
}

impl<'a, B: Backend> Session<'a, B> {
    fn new(backend: &'a B, opened: Opened, thread_id: String, out: Emitter) -> Self {
        Self {
            backend,
            conn: opened.conn,
            events: opened.events,
            thread_id,
            turn_id: None,
            final_message: String::new(),
            out,
        }
    }

    async fn read_thread(&self, include_turns: bool) -> Result<Thread> {
        self.backend
            .read_thread(&self.conn, &self.thread_id, include_turns)
            .await
    }

    fn event_context(&self, reply_pending: bool) -> EventContext<'_> {
        EventContext {
            thread_id: &self.thread_id,
            turn_id: self.turn_id.as_deref(),
            reply_pending,
        }
    }

    async fn resume(&mut self) -> Result<Option<i32>> {
        let thread = self
            .backend
            .resume_thread(&self.conn, &self.thread_id)
            .await?;
        if thread.running {
            return Ok(None);
        }
        let thread = self.read_thread(true).await?;
        self.final_message = thread.final_message;
        let Some(last) = thread.turns.last() else {
            self.out.emit(
                &json!({"event":"turn", "status":"completed", "threadId":self.thread_id,
                "turnId":Value::Null, "finalMessage":""}),
            );
            return Ok(Some(EXIT_COMPLETED));
        };
        self.turn_id = last.id.clone();
        Ok(Some(self.emit_turn(
            last.status.as_str().unwrap_or("completed"),
            last.fields.clone(),
        )))
    }

    fn emit_turn(&mut self, status: &str, fields: Value) -> i32 {
        let mut event = json!({"event":"turn", "status":status, "threadId":self.thread_id,
            "turnId":self.turn_id, "finalMessage":self.final_message});
        extend_fields(&mut event, fields);
        self.out.emit(&event);
        if status == "completed" {
            EXIT_COMPLETED
        } else {
            EXIT_TURN_NOT_COMPLETED
        }
    }

    async fn reply_to_only_request(
        &mut self,
        payload: ReplyPayload<B::Decision>,
        opts: &WaitFlags,
    ) -> Result<i32> {
        self.backend.subscribe(&self.conn, &self.thread_id).await?;
        // There is no end-of-redelivery marker. Collect the full window, even
        // across terminal notifications, without allowing wait flags to shorten it.
        let until = Instant::now() + Duration::from_secs(REPLY_REDELIVERY_SECS);
        let mut requests: Vec<(Value, String)> = Vec::new();
        let mut notifications = Vec::new();
        let mut resolved = Vec::new();
        let mut last_item = Instant::now();
        let mut queued_at_deadline = None;
        loop {
            let raw = if let Some(remaining) = queued_at_deadline.as_mut() {
                if *remaining == 0 {
                    break;
                }
                *remaining -= 1;
                self.events.try_recv().ok()
            } else {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(until) => {
                        queued_at_deadline = Some(self.events.len());
                        continue;
                    }
                    event = self.events.recv() => event,
                }
            }
            .ok_or_else(|| anyhow!("event stream ended unexpectedly"))?;
            if self.backend.defer_during_selection(&raw) {
                notifications.push(raw);
                continue;
            }
            let event = self
                .backend
                .normalize(&self.conn, raw, self.event_context(false))
                .await?;
            match event {
                Event::Request {
                    id,
                    method,
                    thread_id,
                    ..
                } => {
                    if thread_id.as_deref().is_some_and(|id| id != self.thread_id) {
                        continue;
                    }
                    if !requests
                        .iter()
                        .any(|(seen, _)| self.backend.same_request_id(seen, &id))
                    {
                        requests.push((id, method));
                    }
                }
                Event::Closed { reason } => bail!("websocket closed: {reason}"),
                event => {
                    if let Event::RequestResolved { id, .. } = &event {
                        resolved.push(id.clone());
                    }
                    self.handle(event, opts, &mut None, &mut last_item).await?;
                }
            }
        }
        match requests.len() {
            0 => bail!("no pending server request was re-delivered within {REPLY_REDELIVERY_SECS}s of resuming thread {}", self.thread_id),
            1 => {},
            count => bail!("{count} pending server requests were re-delivered for thread {}; use --request-id to choose one", self.thread_id),
        }
        let (id, method) = requests.pop().expect("exactly one request");
        if resolved
            .iter()
            .any(|seen| self.backend.same_request_id(seen, &id))
        {
            bail!("server request {id} was already resolved; no reply sent");
        }
        self.answer_request(&id, &method, &payload).await?;
        for raw in notifications {
            let event = self
                .backend
                .normalize(&self.conn, raw, self.event_context(false))
                .await?;
            if let Step::Exit(code) = self.handle(event, opts, &mut None, &mut last_item).await? {
                return Ok(code);
            }
        }
        self.wait_loop(opts, None).await
    }

    async fn answer_request(
        &mut self,
        id: &Value,
        method: &str,
        payload: &ReplyPayload<B::Decision>,
    ) -> Result<()> {
        let body = match payload {
            ReplyPayload::Decision(decision) => {
                self.backend.decision_response(method, *decision)?
            }
            ReplyPayload::ResultJson(value) => value.clone(),
        };
        self.conn.respond_result(id, body.clone()).await?;
        self.out.log(&json!({"event":"replied", "threadId":self.thread_id, "requestId":id, "method":method, "result":body}));
        Ok(())
    }

    async fn wait_loop(
        &mut self,
        opts: &WaitFlags,
        mut reply: Option<PendingReply<B::Decision>>,
    ) -> Result<i32> {
        let started = Instant::now();
        let mut last_item = Instant::now();
        loop {
            let wake = next_wake(
                &mut self.events,
                B::WAIT_POLICY.deadlines_first,
                deadline(last_item, opts.stall_secs),
                deadline(started, opts.timeout_secs),
                reply
                    .as_ref()
                    .map(|r| r.deadline)
                    .unwrap_or_else(|| Instant::now() + NEVER),
            )
            .await;
            let step = match wake {
                Wake::Event(Some(raw)) => {
                    let event = self
                        .backend
                        .normalize(&self.conn, raw, self.event_context(reply.is_some()))
                        .await?;
                    self.handle(event, opts, &mut reply, &mut last_item).await?
                }
                Wake::Event(None) => {
                    self.out.emit(
                        &json!({"event":"error", "message":"event stream ended unexpectedly"}),
                    );
                    Step::Exit(EXIT_ERROR)
                }
                Wake::Reply => {
                    let id = reply.as_ref().map(|r| r.id.clone()).unwrap_or(Value::Null);
                    self.out.emit(&json!({"event":"error", "message":format!(
                        "server request {id} was not re-delivered within {REPLY_REDELIVERY_SECS}s of resuming thread {}", self.thread_id)}));
                    Step::Exit(EXIT_ERROR)
                }
                Wake::Stall => {
                    self.out.emit(
                        &json!({"event":"stalled", "threadId":self.thread_id, "turnId":self.turn_id,
                        "stallSecs":opts.stall_secs, "finalMessage":self.final_message}),
                    );
                    if opts.follow {
                        last_item = Instant::now();
                        Step::Continue
                    } else {
                        Step::Exit(EXIT_STALLED)
                    }
                }
                Wake::Timeout => {
                    self.out.emit(
                        &json!({"event":"timeout", "threadId":self.thread_id, "turnId":self.turn_id,
                        "timeoutSecs":opts.timeout_secs, "finalMessage":self.final_message}),
                    );
                    Step::Exit(EXIT_STALLED)
                }
            };
            if let Step::Exit(code) = step {
                return Ok(code);
            }
        }
    }

    async fn handle(
        &mut self,
        event: Event,
        opts: &WaitFlags,
        reply: &mut Option<PendingReply<B::Decision>>,
        last_item: &mut Instant,
    ) -> Result<Step> {
        self.out.suppress_log = self.out.unlogged_events > 0;
        self.out.unlogged_events = self.out.unlogged_events.saturating_sub(1);
        let result = self.handle_event(event, opts, reply, last_item).await;
        self.out.suppress_log = false;
        result
    }

    async fn handle_event(
        &mut self,
        event: Event,
        opts: &WaitFlags,
        reply: &mut Option<PendingReply<B::Decision>>,
        last_item: &mut Instant,
    ) -> Result<Step> {
        let policy = &B::WAIT_POLICY;
        match event {
            Event::Ignore => {}
            Event::Closed { reason } => {
                self.out.emit(
                    &json!({"event":"error", "message":format!("websocket closed: {reason}")}),
                );
                return Ok(Step::Exit(EXIT_ERROR));
            }
            Event::Update {
                turn_id,
                final_message,
                progress,
                log,
            } => {
                if turn_id.is_some() {
                    self.turn_id = turn_id;
                }
                if let Some(text) = final_message {
                    self.final_message = text;
                }
                if progress {
                    *last_item = Instant::now();
                }
                if let Some(log) = log {
                    self.out.log(&log);
                }
            }
            Event::TurnCompleted {
                turn_id,
                status,
                final_message,
                fields,
            } => {
                if turn_id.is_some() {
                    self.turn_id = turn_id;
                }
                if let Some(text) = final_message {
                    self.final_message = text;
                }
                return Ok(Step::Exit(self.emit_turn(&status, fields)));
            }
            Event::RequestResolved { id, log } => {
                self.out.log(&log);
                if reply
                    .as_ref()
                    .is_some_and(|r| self.backend.same_request_id(&r.id, &id))
                {
                    bail!("pending server request was already resolved; no reply sent");
                }
            }
            Event::Request {
                id, method, params, ..
            } => {
                if reply
                    .as_ref()
                    .is_some_and(|r| self.backend.same_request_id(&r.id, &id))
                {
                    let pending = reply.take().expect("matched request");
                    self.answer_request(&id, &method, &pending.payload).await?;
                    if policy.request_is_progress {
                        *last_item = Instant::now();
                    }
                    return Ok(Step::Continue);
                }
                if reply.is_some() && policy.isolate_selected_request {
                    return Ok(Step::Continue);
                }
                if opts.auto_decline {
                    let (code, message) = self.backend.unattended();
                    self.conn.respond_error(&id, code, message).await?;
                    self.out.log(&json!({"event":"autoDeclined", "threadId":self.thread_id, "requestId":id, "method":method, "message":message}));
                    if policy.request_is_progress {
                        *last_item = Instant::now();
                    }
                    return Ok(Step::Continue);
                }
                self.out.emit(&json!({"event":"request", "threadId":params.get("threadId").cloned().unwrap_or_else(|| json!(self.thread_id)),
                    "requestId":id, "method":method, "params":params}));
                if !opts.follow {
                    return Ok(Step::Exit(EXIT_REQUEST_PENDING));
                }
            }
        }
        Ok(Step::Continue)
    }
}

fn deadline(from: Instant, secs: u64) -> Instant {
    if secs == 0 {
        Instant::now() + NEVER
    } else {
        from + Duration::from_secs(secs)
    }
}

enum Wake {
    Event(Option<rpc::Event>),
    Reply,
    Stall,
    Timeout,
}

/// Scheduling precedence is part of the backend's existing wait contract.
async fn next_wake(
    events: &mut UnboundedReceiver<rpc::Event>,
    deadlines_first: bool,
    stall: Instant,
    timeout: Instant,
    reply: Instant,
) -> Wake {
    if deadlines_first {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(timeout) => Wake::Timeout,
            _ = tokio::time::sleep_until(reply) => Wake::Reply,
            _ = tokio::time::sleep_until(stall) => Wake::Stall,
            event = events.recv() => Wake::Event(event),
        }
    } else {
        tokio::select! {
            biased;
            event = events.recv() => Wake::Event(event),
            _ = tokio::time::sleep_until(reply) => Wake::Reply,
            _ = tokio::time::sleep_until(stall) => Wake::Stall,
            _ = tokio::time::sleep_until(timeout) => Wake::Timeout,
        }
    }
}
