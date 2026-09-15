//! One transport-only CLI, with independent backend command definitions.

mod backend;
mod cli;
mod core;

use backend::{codex, dsh};
use clap::{Parser, Subcommand};
use core::{output, session::EXIT_ERROR};
use serde_json::json;

#[derive(Parser, Debug)]
#[command(
    name = "agent-bridge",
    version,
    propagate_version = true,
    about = "Drive local agent backends. stdout is JSON, one object per line."
)]
struct Cli {
    #[command(subcommand)]
    backend: BackendCommand,
}

#[derive(Subcommand, Debug)]
enum BackendCommand {
    /// Drive the app-server.
    Codex(codex::cli::Cli),
    /// Drive the dsh bridge plugin.
    Dsh(dsh::cli::Cli),
}

#[tokio::main]
async fn main() {
    // Exit 2 means a pending server request; clap usage errors must use 4.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let usage = err.use_stderr();
            if usage {
                let _ = err.print();
            } else {
                output::check_stdout(err.print());
                output::flush_stdout();
            }
            std::process::exit(if usage { EXIT_ERROR } else { 0 });
        }
    };
    let result = match cli.backend {
        BackendCommand::Codex(args) => {
            let global = cli::Global {
                url: args.url,
                log: args.log.log,
                no_log: args.log.no_log,
            };
            cli::dispatch(
                &codex::Codex {
                    no_pane: args.no_pane,
                },
                &global,
                args.command,
            )
            .await
        }
        BackendCommand::Dsh(args) => {
            let global = cli::Global {
                url: args.url,
                log: args.log.log,
                no_log: args.log.no_log,
            };
            cli::dispatch(&dsh::Dsh, &global, args.command).await
        }
    };
    let code = match result {
        Ok(code) => code,
        Err(err) => {
            output::write_line(json!({"event":"error", "message":format!("{err:#}")}));
            eprintln!("agent-bridge: {err:?}");
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
