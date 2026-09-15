//! The Herdr pane: a human-visible Codex TUI attached to the thread.
//!
//! Best effort by construction. Every failure is one line on stderr and nothing
//! else: the pane never touches stdout or the exit code. Nothing here closes a
//! pane either — the human presses Ctrl+C in the TUI and closes it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// How long the freshly split pane's shell gets before the agent is attached.
const SHELL_SETTLE_MS: u64 = 2_000;

/// How long herdr may spend on `agent start`. Short on purpose: herdr counts a
/// start as successful only once the agent accepts input, and a thread resumed
/// mid-turn never does before the turn ends. Detecting codex in the pane is all
/// this needs, so the wait is cut short and its outcome is not load bearing.
const START_TIMEOUT_MS: &str = "5000";

/// Pause between `agent rename` attempts, and how many retries follow the first
/// call: detection may still be in flight when `agent start` returns.
const RENAME_RETRY_MS: u64 = 500;
const RENAME_RETRIES: u32 = 10;

/// The herdr executable, when a pane is wanted at all.
///
/// `None` means "no pane": `--no-pane`, not running inside Herdr, or no herdr
/// executable to call. `AGENT_BRIDGE_CODEX_HERDR_BIN` substitutes a fake in tests.
pub fn herdr_bin(no_pane: bool) -> Option<PathBuf> {
    if no_pane || std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
        return None;
    }
    if let Ok(explicit) = std::env::var("AGENT_BRIDGE_CODEX_HERDR_BIN") {
        return (!explicit.is_empty()).then(|| PathBuf::from(explicit));
    }
    which("herdr")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions.iter().find_map(|ext| {
            let candidate = dir.join(format!("{name}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Herdr agent names are `codex-` plus the thread id's first 8 characters.
pub fn agent_name(thread_id: &str) -> String {
    format!("codex-{}", thread_id.chars().take(8).collect::<String>())
}

fn settle_ms() -> u64 {
    env_ms("AGENT_BRIDGE_CODEX_PANE_DELAY_MS", SHELL_SETTLE_MS)
}

fn rename_retry_ms() -> u64 {
    env_ms("AGENT_BRIDGE_CODEX_PANE_RETRY_MS", RENAME_RETRY_MS)
}

fn env_ms(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One line on stderr, which is all any pane trouble ever earns.
fn warn(message: &str) {
    eprintln!("agent-bridge codex: herdr pane: {message}");
}

async fn herdr<I, S>(bin: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {}", bin.display()))
}

/// Open a pane for `thread_id`, or reuse the one that is already there.
///
/// Three ways it can go: the agent is still attached and nothing is needed; the
/// TUI is gone but its pane survives under the same name, so codex is started
/// again in it; or there is no pane at all and one is split.
pub async fn open(bin: &Path, thread_id: &str, cwd: &str, ws_url: &str) -> Result<()> {
    let name = agent_name(thread_id);
    if herdr(bin, ["agent", "get", &name]).await?.status.success() {
        return Ok(()); // Someone is already watching this thread.
    }

    // The agent name dies with the TUI, the pane label does not. A pane still
    // carrying the name is the pane this thread was watched in, so codex goes
    // back into it instead of onto a second pane beside the empty one.
    if let Some(pane_id) = pane_by_label(bin, &name).await {
        attach(bin, &pane_id, &name, thread_id, ws_url).await;
        return Ok(());
    }

    let split = herdr(
        bin,
        [
            "pane",
            "split",
            "--current",
            "--direction",
            "right",
            "--ratio",
            "0.45",
            "--cwd",
            cwd,
        ],
    )
    .await?;
    if !split.status.success() {
        bail!(
            "herdr pane split failed: {}",
            String::from_utf8_lossy(&split.stderr).trim()
        );
    }
    let pane_id = serde_json::from_slice::<Value>(&split.stdout)
        .ok()
        .and_then(|value| {
            value
                .pointer("/result/pane/pane_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("herdr pane split printed no .result.pane.pane_id"))?;

    // Label the pane straight away: this is what the next call finds once the
    // TUI is gone and `agent get` has nothing left to answer with.
    if let Err(err) = label_pane(bin, &pane_id, &name).await {
        warn(&format!("{err:#}"));
    }

    tokio::time::sleep(Duration::from_millis(settle_ms())).await;
    attach(bin, &pane_id, &name, thread_id, ws_url).await;
    Ok(())
}

/// Start codex in `pane_id` and bind `name` to it. Best effort throughout: a
/// pane that is busy refuses both halves and all that earns is stderr.
async fn attach(bin: &Path, pane_id: &str, name: &str, thread_id: &str, ws_url: &str) {
    match herdr(
        bin,
        [
            "agent",
            "start",
            name,
            "--kind",
            "codex",
            "--pane",
            pane_id,
            "--timeout",
            START_TIMEOUT_MS,
            "--",
            "resume",
            thread_id,
            "--remote",
            ws_url,
        ],
    )
    .await
    {
        Ok(started) if started.status.success() => {}
        Ok(started) => warn(&format!(
            "herdr agent start did not confirm readiness: {}",
            String::from_utf8_lossy(&started.stderr).trim()
        )),
        Err(err) => warn(&format!("herdr agent start failed: {err:#}")),
    }

    // The name is bound here, not by `agent start`: a start that timed out still
    // left codex running in the pane, and without the name the next call finds
    // no agent and splits a second pane. On the happy path the agent already
    // carries the name and this renames it to itself.
    if let Err(err) = rename(bin, pane_id, name).await {
        warn(&format!("{err:#}"));
    }
}

/// The id of the pane labelled `name`, if `herdr pane list` reports one.
///
/// Silent by construction: no list, no match and no parse all mean the same
/// thing to the caller, which is "split a pane".
async fn pane_by_label(bin: &Path, name: &str) -> Option<String> {
    let listed = herdr(bin, ["pane", "list"]).await.ok()?;
    if !listed.status.success() {
        return None;
    }
    let value = serde_json::from_slice::<Value>(&listed.stdout).ok()?;
    value
        .pointer("/result/panes")?
        .as_array()?
        .iter()
        .find(|pane| pane.get("label").and_then(Value::as_str) == Some(name))
        .and_then(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_string)
}

/// Label the pane itself, which outlives the agent attached to it.
async fn label_pane(bin: &Path, pane_id: &str, name: &str) -> Result<()> {
    let output = herdr(bin, ["pane", "rename", pane_id, name]).await?;
    if !output.status.success() {
        bail!(
            "herdr pane rename {pane_id} {name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Bind `name` to the pane, retrying while herdr is still making up its mind.
async fn rename(bin: &Path, pane_id: &str, name: &str) -> Result<()> {
    let mut last = String::new();
    for attempt in 0..=RENAME_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(rename_retry_ms())).await;
        }
        match herdr(bin, ["agent", "rename", pane_id, name]).await {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => last = String::from_utf8_lossy(&output.stderr).trim().to_string(),
            Err(err) => last = format!("{err:#}"),
        }
    }
    bail!(
        "herdr agent rename {pane_id} {name} failed {} times: {last}",
        RENAME_RETRIES + 1
    )
}

#[cfg(test)]
mod tests {
    use super::agent_name;

    #[test]
    fn the_agent_name_is_the_thread_id_prefix() {
        assert_eq!(agent_name("0199abcd-ef01-7000-8000-0123"), "codex-0199abcd");
        assert_eq!(agent_name("short"), "codex-short");
    }
}
