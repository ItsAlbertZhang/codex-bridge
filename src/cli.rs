//! Parameters and verbs whose contracts are shared by every backend.

use std::fmt::Debug;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;

use crate::backend::{Backend, RunConfig};
use crate::core::{config, daemon, session};

#[derive(Clone, Debug)]
pub struct Global {
    pub url: String,
    pub log: Option<PathBuf>,
    pub no_log: bool,
}

#[derive(Args, Debug)]
pub struct LogFlags {
    /// Append JSON lines here instead of <state dir>/logs/<threadId>.jsonl.
    #[arg(long, global = true)]
    pub log: Option<PathBuf>,
    /// Disable the per-thread JSON log.
    #[arg(long, global = true, conflicts_with = "log")]
    pub no_log: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command<R, D, I>
where
    R: Args + Debug,
    D: ValueEnum + Debug + Send + Sync + 'static,
    I: Clone + Debug + Send + Sync + FromStr + 'static,
    I::Err: std::error::Error + Send + Sync + 'static,
{
    /// Open a thread, start a turn, and (unless --no-wait) wait for it.
    Run(R),
    /// Resume a thread and wait for its next interesting event.
    Wait(WaitArgs),
    /// Answer a pending server request, then keep waiting.
    Reply(ReplyArgs<D, I>),
    /// Add input to the turn that is currently running.
    Steer(SteerArgs),
    /// Interrupt the turn that is currently running.
    Interrupt(ThreadArg),
    /// Print the thread's status, cwd and model.
    Status(ThreadArg),
    /// Print the thread's turns and its final message.
    Read(ThreadArg),
    /// Inspect or manage the daemon process.
    Daemon(DaemonArgs),
}

#[derive(Args, Debug)]
pub struct ThreadArg {
    #[arg(long = "thread")]
    pub thread: String,
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[arg(value_enum)]
    pub action: daemon::Action,
}

#[derive(Args, Clone, Debug)]
pub struct WaitFlags {
    /// Give up when no main-thread progress occurs for this long. 0 disables.
    #[arg(long, default_value_t = 600)]
    pub stall_secs: u64,
    /// Give up this long after the wait started. 0 disables.
    #[arg(long, default_value_t = 3600)]
    pub timeout_secs: u64,
    /// Answer every server request with an unattended JSON-RPC error.
    #[arg(long)]
    pub auto_decline: bool,
    /// Keep running and print every event as its own JSON line.
    #[arg(long)]
    pub follow: bool,
}

#[derive(Args, Debug)]
#[group(id = "CommonRunArgs")]
pub struct RunArgs {
    /// Start a turn on this existing thread instead of opening a new one.
    #[arg(long = "thread", conflicts_with_all = [
        "cwd", "sandbox", "approval", "model", "effort", "developer_instructions_file"
    ])]
    pub thread: Option<String>,
    /// Working directory for the thread.
    #[arg(long, required_unless_present = "thread")]
    pub cwd: Option<String>,
    /// Prompt text.
    #[arg(
        long,
        conflicts_with = "prompt_file",
        required_unless_present = "prompt_file"
    )]
    pub prompt: Option<String>,
    /// File holding the prompt text.
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
    #[arg(long, value_parser = ["read-only", "workspace-write", "danger-full-access"])]
    pub sandbox: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning effort.
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long)]
    pub developer_instructions_file: Option<PathBuf>,
    /// Print the started event and exit without waiting.
    #[arg(long)]
    pub no_wait: bool,
    #[command(flatten)]
    pub wait: WaitFlags,
}

impl RunArgs {
    pub fn prompt_text(&self) -> Result<String> {
        match (&self.prompt, &self.prompt_file) {
            (Some(text), _) => Ok(text.clone()),
            (None, Some(path)) => std::fs::read_to_string(path)
                .with_context(|| format!("reading prompt file {}", path.display())),
            (None, None) => unreachable!("clap requires a prompt"),
        }
    }

    pub fn developer_instructions(&self) -> Result<Option<String>> {
        self.developer_instructions_file
            .as_ref()
            .map(|path| {
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading developer instructions {}", path.display()))
            })
            .transpose()
    }
}

#[derive(Args, Debug)]
pub struct WaitArgs {
    #[arg(long = "thread")]
    pub thread: String,
    #[command(flatten)]
    pub wait: WaitFlags,
}

#[derive(Args, Debug)]
pub struct ReplyArgs<D, I>
where
    D: ValueEnum + Debug + Send + Sync + 'static,
    I: Clone + Debug + Send + Sync + FromStr + 'static,
    I::Err: std::error::Error + Send + Sync + 'static,
{
    #[arg(long = "thread")]
    pub thread: String,
    /// Omit to select the sole request re-delivered within 10 seconds.
    #[arg(long = "request-id")]
    pub request_id: Option<I>,
    /// Mapped to the response shape required by the request's method.
    #[arg(
        long,
        value_enum,
        conflicts_with = "result_json",
        required_unless_present = "result_json"
    )]
    pub decision: Option<D>,
    /// Sent verbatim as the JSON-RPC result.
    #[arg(long)]
    pub result_json: Option<String>,
    #[command(flatten)]
    pub wait: WaitFlags,
}

#[derive(Args, Debug)]
pub struct SteerArgs {
    #[arg(long = "thread")]
    pub thread: String,
    /// Omit to use the thread's current in-progress turn.
    #[arg(long = "turn")]
    pub turn: Option<String>,
    #[arg(long)]
    pub text: String,
}

pub async fn dispatch<B, I>(
    backend: &B,
    global: &Global,
    command: Command<B::Run, B::Decision, I>,
) -> Result<i32>
where
    B: Backend,
    B::Run: Args + Debug,
    B::Decision: ValueEnum + Debug + Send + Sync + 'static,
    I: Clone + Debug + Send + Sync + FromStr + Into<Value> + 'static,
    I::Err: std::error::Error + Send + Sync + 'static,
{
    let config = config::load()?;
    match command {
        Command::Daemon(args) => daemon::run(backend, &global.url, args.action).await,
        Command::Run(args) => {
            let prompt = args.common().prompt_text()?;
            // Read the file before connecting, including on a resumed thread.
            let instructions = args.common().developer_instructions()?;
            let settings = config.resolve(backend, args.common());
            session::run(
                backend,
                global,
                &args,
                &prompt,
                instructions.as_deref(),
                &settings,
            )
            .await
        }
        Command::Wait(args) => session::wait(backend, global, &args.thread, &args.wait).await,
        Command::Reply(args) => {
            let payload = match (args.decision, args.result_json) {
                (Some(decision), _) => session::ReplyPayload::Decision(decision),
                (None, Some(raw)) => session::ReplyPayload::ResultJson(
                    serde_json::from_str(&raw).context("--result-json is not valid JSON")?,
                ),
                (None, None) => unreachable!("clap requires a reply payload"),
            };
            session::reply(
                backend,
                global,
                &args.thread,
                args.request_id.map(Into::into),
                payload,
                &args.wait,
            )
            .await
        }
        Command::Steer(args) => {
            session::steer(
                backend,
                global,
                &args.thread,
                args.turn.as_deref(),
                &args.text,
            )
            .await
        }
        Command::Interrupt(args) => session::interrupt(backend, global, &args.thread).await,
        Command::Status(args) => session::status(backend, global, &args.thread).await,
        Command::Read(args) => session::read(backend, global, &args.thread).await,
    }
}
