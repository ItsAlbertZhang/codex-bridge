//! Compile-time Codex options; its approval vocabulary never reaches dsh.

use clap::Args;

pub use super::approvals::Decision;

#[derive(Args, Debug)]
pub struct Cli {
    /// WebSocket endpoint of the Codex app-server.
    #[arg(
        long,
        global = true,
        env = "AGENT_BRIDGE_CODEX_URL",
        default_value = "ws://127.0.0.1:12897"
    )]
    pub url: String,
    #[command(flatten)]
    pub log: crate::cli::LogFlags,
    /// Do not open or reuse a Herdr pane for the thread.
    #[arg(long, global = true)]
    pub no_pane: bool,
    #[command(subcommand)]
    pub command: crate::cli::Command<RunArgs, Decision, i64>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: crate::cli::RunArgs,
    /// Codex approval policy for the new thread.
    #[arg(long, value_parser = ["untrusted", "on-request", "never"], conflicts_with = "thread")]
    pub approval: Option<String>,
}

impl super::super::RunConfig for RunArgs {
    fn common(&self) -> &crate::cli::RunArgs {
        &self.common
    }
}
