//! Shared daemon lifecycle: readiness probe, detached spawn, pid file.
//!
//! Every subcommand except `daemon` calls [`ensure`] first, so the caller never
//! has to think about whether the server is up. The server owns all thread and
//! turn state, so starting it is the only thing this module does to it; it is
//! never restarted implicitly.

use crate::backend::Backend;
use crate::core::{extend_fields, output};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

const PID_FILE: &str = "daemon.pid";
const LOG_FILE: &str = "daemon.log";
const DEFAULT_READY_TIMEOUT_MS: u64 = 15_000;
const POLL_MS: u64 = 250;
/// One probe must not hang longer than the poll interval is useful for.
const PROBE_TIMEOUT_MS: u64 = 1_500;

/// Where the pid file and the daemon log live. Created on demand.
pub fn state_dir(backend: &impl Backend) -> PathBuf {
    if let Ok(explicit) = std::env::var("AGENT_BRIDGE_STATE_DIR") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit).join(backend.state_name());
        }
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local").join("state")));
    base.unwrap_or_else(std::env::temp_dir)
        .join("agent-bridge")
        .join(backend.state_name())
}

pub(super) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    const HOME_VAR: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME_VAR: &str = "HOME";
    std::env::var_os(HOME_VAR)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// A daemon has a stable cwd regardless of where its caller happened to run.
fn daemon_cwd(backend: &impl Backend) -> PathBuf {
    home_dir().unwrap_or_else(|| state_dir(backend))
}

fn ready_timeout_ms(backend: &impl Backend) -> u64 {
    std::env::var(format!("{}READY_TIMEOUT_MS", backend.env_prefix()))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_READY_TIMEOUT_MS)
}

/// `ws://host:port` -> `host:port`.
fn authority(url: &str) -> Result<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("{url} has no scheme"))?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        bail!("{url} has no host");
    }
    Ok(authority.to_string())
}

/// The readiness URL: the ws url with the scheme swapped.
pub fn http_url(ws_url: &str) -> String {
    match ws_url.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some((_, rest)) => format!("http://{rest}"),
        None => format!("http://{ws_url}"),
    }
}

/// `GET /readyz` written by hand: one request is not worth an http client.
pub async fn probe_ready(ws_url: &str) -> bool {
    let Ok(authority) = authority(ws_url) else {
        return false;
    };
    let probe = async {
        let mut stream = TcpStream::connect(&authority).await.ok()?;
        let request =
            format!("GET /readyz HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut seen = Vec::new();
        let mut chunk = [0u8; 256];
        while seen.len() < 512 {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&chunk[..n]);
            if seen.contains(&b'\n') {
                break;
            }
        }
        let head = String::from_utf8_lossy(&seen);
        Some(head.lines().next().unwrap_or("").contains(" 200"))
    };
    matches!(
        tokio::time::timeout(Duration::from_millis(PROBE_TIMEOUT_MS), probe).await,
        Ok(Some(true))
    )
}

/// Ready, or started and then ready. Called by every subcommand but `daemon`.
pub async fn ensure(backend: &impl Backend, ws_url: &str) -> Result<()> {
    bring_up(backend, ws_url, false).await.map(|_| ())
}

async fn bring_up(backend: &impl Backend, ws_url: &str, report: bool) -> Result<Value> {
    if probe_ready(ws_url).await {
        return Ok(json!({}));
    }
    let offset = std::fs::metadata(state_dir(backend).join(LOG_FILE))
        .map(|meta| meta.len())
        .unwrap_or(0);
    spawn(backend, ws_url)?;
    let timeout = ready_timeout_ms(backend);
    let deadline = Instant::now() + Duration::from_millis(timeout);
    loop {
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        // Another process may have won the port; readiness is authoritative.
        if probe_ready(ws_url).await {
            return Ok(backend.after_ready(offset, report).await);
        }
        if Instant::now() >= deadline {
            bail!(
                "{}/readyz did not answer within {timeout} ms (see {})",
                http_url(ws_url),
                state_dir(backend).join(LOG_FILE).display()
            );
        }
    }
}

/// A killed process can briefly keep answering. Wait before checking whether
/// a replacement needs to be spawned, bounded by half the readiness budget.
async fn wait_until_down(backend: &impl Backend, ws_url: &str) {
    let budget = ready_timeout_ms(backend) / 2;
    let deadline = Instant::now() + Duration::from_millis(budget);
    loop {
        if !probe_ready(ws_url).await {
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "{} still answers {budget} ms after the stop; starting anyway",
                http_url(ws_url)
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

/// Clear `HANDLE_FLAG_INHERIT` on this process' standard handles, so that a
/// child spawned with `bInheritHandles` cannot pick them up. Handles we do not
/// own (a closed or invalid stream) are skipped; a failure here is not worth
/// failing the spawn over.
#[cfg(windows)]
fn no_inherit_std_handles() {
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: both calls only read and re-flag a handle this process owns.
        unsafe {
            let handle = GetStdHandle(id);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// Spawn the backend command so that it outlives this process.
fn spawn(backend: &impl Backend, ws_url: &str) -> Result<u32> {
    let dir = state_dir(backend);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating state dir {}", dir.display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
        .with_context(|| format!("opening {}", dir.join(LOG_FILE).display()))?;
    let log_err = log
        .try_clone()
        .context("duplicating the daemon log handle")?;

    let mut command = backend.daemon_command(ws_url)?;
    command
        .current_dir(daemon_cwd(backend))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        /// A console the child owns but nobody sees. `DETACHED_PROCESS` would
        /// give the daemon no console at all, and then every console program it
        /// spawns in turn (MCP servers, hook shells) gets a console of
        /// its own — a black window flashing on the user's screen for each one.
        /// With `CREATE_NO_WINDOW` they all inherit this one hidden console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // The daemon outlives us, so it must not keep our standard handles
        // alive: if our stdout is a pipe, whoever reads it would wait for EOF
        // until the daemon exits. We cannot hand `Command` a handle list, so we
        // take the inherit flag off our own handles instead. Nothing else needs
        // them inheritable — every other child we spawn brings its own stdio.
        no_inherit_std_handles();
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches nothing we own.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawning daemon {command:?}"))?;
    let pid = child.id();
    let _ = std::fs::write(dir.join(PID_FILE), pid.to_string());
    Ok(pid)
}

fn read_pid(backend: &impl Backend) -> Option<u32> {
    std::fs::read_to_string(state_dir(backend).join(PID_FILE))
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
}

fn clear_pid(backend: &impl Backend) {
    let _ = std::fs::remove_file(state_dir(backend).join(PID_FILE));
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

#[cfg(windows)]
fn terminate(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for the process' existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
fn terminate(pid: u32) -> bool {
    // SAFETY: plain SIGTERM to a pid we wrote ourselves.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

/// `{"url","ready","managed","pid"?}`.
pub async fn status_value(backend: &impl Backend, ws_url: &str, fresh: Value) -> Value {
    let pid = read_pid(backend);
    let managed = pid.is_some_and(alive);
    let mut value = json!({
        "url": ws_url,
        "ready": probe_ready(ws_url).await,
        "managed": managed,
    });
    if let Some(pid) = pid {
        value["pid"] = json!(pid);
    }
    extend_fields(&mut value, backend.daemon_fields());
    extend_fields(&mut value, fresh);
    value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Action {
    Status,
    Start,
    Stop,
    Restart,
}

fn emit(value: &Value) {
    output::write_line(value);
}

pub async fn run(backend: &impl Backend, ws_url: &str, action: Action) -> Result<i32> {
    match action {
        Action::Status => {
            emit(&status_value(backend, ws_url, json!({})).await);
            Ok(0)
        }
        Action::Start => {
            let fresh = bring_up(backend, ws_url, true).await?;
            emit(&status_value(backend, ws_url, fresh).await);
            Ok(0)
        }
        Action::Stop => stop(backend, ws_url).await,
        Action::Restart => {
            let code = stop(backend, ws_url).await?;
            if code != 0 {
                return Ok(code);
            }
            wait_until_down(backend, ws_url).await;
            let fresh = bring_up(backend, ws_url, true).await?;
            emit(&status_value(backend, ws_url, fresh).await);
            Ok(0)
        }
    }
}

/// Refuses a server we did not start: whoever launched it by hand stops it.
async fn stop(backend: &impl Backend, ws_url: &str) -> Result<i32> {
    match read_pid(backend) {
        Some(pid) => {
            if alive(pid) {
                terminate(pid);
            }
            clear_pid(backend);
            emit(&json!({ "event": "stopped", "url": ws_url, "pid": pid }));
            Ok(0)
        }
        None if probe_ready(ws_url).await => {
            emit(&json!({
                "event": "error",
                "message": format!("server at {ws_url} is not managed by agent-bridge"),
            }));
            Ok(crate::core::session::EXIT_ERROR)
        }
        None => {
            emit(&json!({ "event": "stopped", "url": ws_url, "pid": Value::Null }));
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_readiness_url_is_the_ws_url_with_another_scheme() {
        assert_eq!(http_url("ws://127.0.0.1:12897"), "http://127.0.0.1:12897");
        assert_eq!(http_url("wss://host/path"), "https://host/path");
        assert_eq!(http_url("127.0.0.1:1"), "http://127.0.0.1:1");
    }

    #[test]
    fn the_authority_is_host_and_port_only() {
        assert_eq!(
            authority("ws://127.0.0.1:12897").unwrap(),
            "127.0.0.1:12897"
        );
        assert_eq!(authority("ws://host:1/readyz").unwrap(), "host:1");
        assert!(authority("127.0.0.1:1").is_err());
    }
}
