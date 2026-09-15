//! Compile-time dsh options and its one-time approval vocabulary.

use clap::Args;

pub use super::approvals::Decision;

#[derive(Args, Debug)]
pub struct Cli {
    /// WebSocket endpoint of the dsh bridge plugin.
    #[arg(
        long,
        global = true,
        env = "AGENT_BRIDGE_DSH_URL",
        default_value = "ws://127.0.0.1:12898"
    )]
    pub url: String,
    #[command(flatten)]
    pub log: crate::cli::LogFlags,
    #[command(subcommand)]
    pub command: crate::cli::Command<RunArgs, Decision, String>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: crate::cli::RunArgs,
    /// dsh approval policy for the new thread.
    #[arg(long, value_parser = ["ask", "never"], conflicts_with = "thread")]
    pub approval: Option<String>,
    /// Human-readable title for the new thread.
    #[arg(long, conflicts_with = "thread")]
    pub title: Option<String>,
}

impl super::super::RunConfig for RunArgs {
    fn common(&self) -> &crate::cli::RunArgs {
        &self.common
    }
}
