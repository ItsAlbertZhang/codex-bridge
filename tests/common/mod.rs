//! Shared scaffolding: a handler-driven in-process HTTP/WebSocket mock server,
//! a scratch state directory, and fake `codex`, `node`, and `herdr` executables.
//!
//! Nothing here ever starts a real backend or Herdr process.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;

pub const BIN: &str = env!("CARGO_BIN_EXE_agent-bridge");

pub type Tx = UnboundedSender<Value>;
pub type Handler = Arc<dyn Fn(&Value, &Tx) + Send + Sync>;

// ------------------------------------------------------------------ scratch dir

/// A directory that removes itself, so the tests never touch the real state dir.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&root).expect("scratch root");
        let path = root.join(format!(
            "agent-bridge-test-{}-{tag}-{n}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("scratch dir");
        std::fs::create_dir(path.join("codex")).expect("codex state dir");
        std::fs::create_dir(path.join("dsh")).expect("dsh state dir");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0
            .join(name.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        if let (Ok(root), Ok(path)) = (root.canonicalize(), self.0.canonicalize()) {
            if path.starts_with(&root) && path != root {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

// --------------------------------------------------------------- fake binaries

/// Write an executable stand-in for `node` that records its arguments in
/// `$FAKE_NODE_LOG` and exits at once, so it never becomes ready.
pub fn fake_node(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-node",
        "@echo off\r\n\
         :record\r\n\
         if \"%~1\"==\"\" exit /b 0\r\n\
         >>\"%FAKE_NODE_LOG%\" echo(%~1\r\n\
         shift\r\n\
         goto record\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" >>\"$FAKE_NODE_LOG\"\n\
         exit 0\n",
    )
}

/// A `node` stand-in that records its arguments and then stays alive for a few
/// seconds, long enough to outlive the CLI that spawned it. It never listens,
/// so the readiness poll still gives up.
pub fn fake_lingering_node(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-lingering-node",
        // `ping`, not `timeout`: the daemon's stdin is null and `timeout`
        // refuses to run with redirected input.
        "@echo off\r\n\
         :record\r\n\
         if \"%~1\"==\"\" goto linger\r\n\
         >>\"%FAKE_NODE_LOG%\" echo(%~1\r\n\
         shift\r\n\
         goto record\r\n\
         :linger\r\n\
         ping -n 5 127.0.0.1 >nul\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" >>\"$FAKE_NODE_LOG\"\n\
         sleep 4\n\
         exit 0\n",
    )
}

/// A `node` stand-in that records its arguments, stays quiet for a few
/// seconds, and then prints a `dsh web:` line. It opens the readiness gate
/// first, like the real dsh prints its URL only after `/readyz` already answers.
pub fn fake_late_web_node(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-late-web-node",
        // `ping`, not `timeout`: the daemon's stdin is null and `timeout`
        // refuses to run with redirected input.
        "@echo off\r\n\
         :record\r\n\
         if \"%~1\"==\"\" goto late\r\n\
         >>\"%FAKE_NODE_LOG%\" echo(%~1\r\n\
         shift\r\n\
         goto record\r\n\
         :late\r\n\
         >\"%FAKE_NODE_MARKER%\" echo ready\r\n\
         ping -n 4 127.0.0.1 >nul\r\n\
         echo dsh web: http://127.0.0.1:12899/?token=NEW\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" >>\"$FAKE_NODE_LOG\"\n\
         echo ready >\"$FAKE_NODE_MARKER\"\n\
         sleep 3\n\
         echo dsh web: http://127.0.0.1:12899/?token=NEW\n\
         exit 0\n",
    )
}

/// A `node` stand-in that records its arguments, prints two `dsh web:` lines,
/// and only then becomes ready (it creates `$FAKE_NODE_MARKER`) and exits.
/// Both lines are in the log before the port ever answers, so the caller can
/// only tell them apart by where it started reading: NEW1 is this launch's
/// first line, and it is reachable only from an offset taken before the spawn.
pub fn fake_two_web_node(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-two-web-node",
        "@echo off\r\n\
         :record\r\n\
         if \"%~1\"==\"\" goto first\r\n\
         >>\"%FAKE_NODE_LOG%\" echo(%~1\r\n\
         shift\r\n\
         goto record\r\n\
         :first\r\n\
         echo dsh web: http://127.0.0.1:12899/?token=NEW1\r\n\
         echo dsh web: http://127.0.0.1:12899/?token=NEW2\r\n\
         >\"%FAKE_NODE_MARKER%\" echo ready\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" >>\"$FAKE_NODE_LOG\"\n\
         echo dsh web: http://127.0.0.1:12899/?token=NEW1\n\
         echo dsh web: http://127.0.0.1:12899/?token=NEW2\n\
         echo ready >\"$FAKE_NODE_MARKER\"\n\
         exit 0\n",
    )
}

/// A `node` stand-in that records its arguments, creates the file named by
/// `$FAKE_NODE_MARKER` — what [`marker_readyz`] and [`gated_mock`] answer 200
/// for — optionally prints a `dsh web:` line, and exits. With `web` it stands
/// in for the daemon a `restart` brings up; without it, for one that becomes
/// ready and never prints a URL at all.
pub fn fake_marker_node(dir: &TempDir, web: Option<&str>) -> PathBuf {
    let (stem, windows_web, unix_web) = match web {
        Some(url) => (
            "fake-marker-web-node",
            format!("echo dsh web: {url}\r\n"),
            format!("echo dsh web: {url}\n"),
        ),
        None => ("fake-marker-node", String::new(), String::new()),
    };
    write_script(
        dir,
        stem,
        &format!(
            "@echo off\r\n\
             :record\r\n\
             if \"%~1\"==\"\" goto ready\r\n\
             >>\"%FAKE_NODE_LOG%\" echo(%~1\r\n\
             shift\r\n\
             goto record\r\n\
             :ready\r\n\
             >\"%FAKE_NODE_MARKER%\" echo ready\r\n\
             {windows_web}exit /b 0\r\n"
        ),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" >>\"$FAKE_NODE_LOG\"\n\
             echo ready >\"$FAKE_NODE_MARKER\"\n\
             {unix_web}exit 0\n"
        ),
    )
}

/// Record the launch directory using the platform shell's own cwd variable.
pub fn fake_cwd(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-cwd",
        "@echo off\r\n>\"%FAKE_CWD_LOG%\" echo %CD%\r\nexit /b 0\r\n",
        "#!/bin/sh\nprintf '%s\\n' \"$PWD\" >\"$FAKE_CWD_LOG\"\nexit 0\n",
    )
}

/// A fake Codex launcher that makes the replacement daemon ready.
pub fn fake_marker_codex(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-marker-codex",
        "@echo off\r\necho %* >>\"%FAKE_CODEX_LOG%\"\r\n>\"%FAKE_CODEX_MARKER%\" echo ready\r\nexit /b 0\r\n",
        "#!/bin/sh\necho \"$@\" >>\"$FAKE_CODEX_LOG\"\necho ready >\"$FAKE_CODEX_MARKER\"\nexit 0\n",
    )
}

fn write_script(dir: &TempDir, stem: &str, windows: &str, unix: &str) -> PathBuf {
    let path = dir.join(&format!(
        "{stem}{}",
        if cfg!(windows) { ".cmd" } else { ".sh" }
    ));
    std::fs::write(&path, if cfg!(windows) { windows } else { unix }).expect("writing the fake");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

/// Write an executable stand-in for `codex` that records its arguments in
/// `$FAKE_CODEX_LOG` and exits at once, so it never becomes ready.
pub fn fake_codex(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-codex",
        "@echo off\r\n\
         echo %* >>\"%FAKE_CODEX_LOG%\"\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         echo \"$@\" >>\"$FAKE_CODEX_LOG\"\n\
         exit 0\n",
    )
}

/// Write an executable stand-in for `herdr`. It records every invocation in
/// `$FAKE_HERDR_LOG`, answers `agent get` with `$FAKE_HERDR_AGENT_GET`, prints
/// `$FAKE_HERDR_PANE_LIST` (a JSON blob; unset means `pane list` prints
/// nothing) for `pane list`, succeeds at `pane rename`, fails
/// `pane split` when `$FAKE_HERDR_SPLIT_FAIL` is `1`, fails `agent start` when
/// `$FAKE_HERDR_START_FAIL` is `1`, and fails the first
/// `$FAKE_HERDR_RENAME_FAILS` `agent rename` calls, counting them in the file
/// `$FAKE_HERDR_RENAME_COUNT`.
pub fn fake_herdr(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-herdr",
        // Flat and goto-driven on purpose: `exit /b <n>` inside a parenthesised
        // batch block does not set the script's exit code. The counter is
        // written with the redirection first, so a bare digit is not taken for
        // a stream number.
        "@echo off\r\n\
         echo %* >>\"%FAKE_HERDR_LOG%\"\r\n\
         if \"%1 %2\"==\"agent get\" exit /b %FAKE_HERDR_AGENT_GET%\r\n\
         if \"%1 %2\"==\"agent start\" goto start\r\n\
         if \"%1 %2\"==\"agent rename\" goto rename\r\n\
         if \"%1 %2\"==\"pane rename\" exit /b 0\r\n\
         if \"%1 %2\"==\"pane list\" goto list\r\n\
         if not \"%1 %2\"==\"pane split\" exit /b 0\r\n\
         if \"%FAKE_HERDR_SPLIT_FAIL%\"==\"1\" exit /b 3\r\n\
         echo {\"result\":{\"pane\":{\"pane_id\":\"P9\"}}}\r\n\
         exit /b 0\r\n\
         :list\r\n\
         if \"%FAKE_HERDR_PANE_LIST%\"==\"\" exit /b 0\r\n\
         echo %FAKE_HERDR_PANE_LIST%\r\n\
         exit /b 0\r\n\
         :start\r\n\
         if \"%FAKE_HERDR_START_FAIL%\"==\"1\" exit /b 7\r\n\
         exit /b 0\r\n\
         :rename\r\n\
         if \"%FAKE_HERDR_RENAME_FAILS%\"==\"\" exit /b 0\r\n\
         set /a COUNT=0\r\n\
         if exist \"%FAKE_HERDR_RENAME_COUNT%\" set /p COUNT=<\"%FAKE_HERDR_RENAME_COUNT%\"\r\n\
         set /a COUNT+=1\r\n\
         >\"%FAKE_HERDR_RENAME_COUNT%\" echo %COUNT%\r\n\
         if %COUNT% LEQ %FAKE_HERDR_RENAME_FAILS% exit /b 5\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         echo \"$@\" >>\"$FAKE_HERDR_LOG\"\n\
         [ \"$1 $2\" = \"agent get\" ] && exit \"$FAKE_HERDR_AGENT_GET\"\n\
         if [ \"$1 $2\" = \"agent start\" ]; then\n\
           [ \"$FAKE_HERDR_START_FAIL\" = \"1\" ] && exit 7\n\
           exit 0\n\
         fi\n\
         if [ \"$1 $2\" = \"agent rename\" ]; then\n\
           [ -z \"$FAKE_HERDR_RENAME_FAILS\" ] && exit 0\n\
           count=0\n\
           [ -f \"$FAKE_HERDR_RENAME_COUNT\" ] && count=$(cat \"$FAKE_HERDR_RENAME_COUNT\")\n\
           count=$((count + 1))\n\
           echo \"$count\" >\"$FAKE_HERDR_RENAME_COUNT\"\n\
           [ \"$count\" -le \"$FAKE_HERDR_RENAME_FAILS\" ] && exit 5\n\
           exit 0\n\
         fi\n\
         [ \"$1 $2\" = \"pane rename\" ] && exit 0\n\
         if [ \"$1 $2\" = \"pane list\" ]; then\n\
           [ -n \"$FAKE_HERDR_PANE_LIST\" ] && echo \"$FAKE_HERDR_PANE_LIST\"\n\
           exit 0\n\
         fi\n\
         [ \"$1 $2\" = \"pane split\" ] || exit 0\n\
         [ \"$FAKE_HERDR_SPLIT_FAIL\" = \"1\" ] && exit 3\n\
         echo '{\"result\":{\"pane\":{\"pane_id\":\"P9\"}}}'\n\
         exit 0\n",
    )
}

/// A `codex` stand-in that records its arguments and then stays alive for a few
/// seconds, long enough to outlive the CLI that spawned it. It never listens,
/// so the readiness poll still gives up.
pub fn fake_lingering_codex(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "fake-lingering-codex",
        // `ping`, not `timeout`: the daemon's stdin is null and `timeout`
        // refuses to run with redirected input.
        "@echo off\r\n\
         echo %* >>\"%FAKE_CODEX_LOG%\"\r\n\
         ping -n 5 127.0.0.1 >nul\r\n\
         exit /b 0\r\n",
        "#!/bin/sh\n\
         echo \"$@\" >>\"$FAKE_CODEX_LOG\"\n\
         sleep 4\n\
         exit 0\n",
    )
}

// ------------------------------------------------------------------ mock server

/// Everything the mock server received, in order.
#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<Value>>>);

impl Recorder {
    pub fn push(&self, value: Value) {
        self.0.lock().unwrap().push(value);
    }

    pub fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }

    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|m| m.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }
}

/// Allocate only within a test range, without asking the OS for a potentially
/// reserved backend port through port zero.
fn test_listener() -> std::net::TcpListener {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    const FIRST: u64 = 30_000;
    const COUNT: u64 = 30_000;
    let seed = u64::from(std::process::id()) * 97;
    for _ in 0..COUNT {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed).wrapping_add(seed) % COUNT;
        let port = (FIRST + offset) as u16;
        match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return listener,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => panic!("binding mock port {port}: {error}"),
        }
    }
    panic!("no free mock port in 30000..60000");
}

/// Bind a mock server on a free port and serve it from a background runtime.
/// It answers `GET /readyz` with 200 so the daemon check passes, and speaks the
/// protocol chosen by its handler on every other connection.
pub fn start_mock(handler: Handler) -> u16 {
    gated_mock(handler, None)
}

/// [`start_mock`], but `/readyz` answers 503 until `gate` exists. The fake launcher
/// creates that file, so the CLI's first probe fails and the spawn path runs —
/// without the test having to beat a wall clock to it.
pub fn gated_mock(handler: Handler, gate: Option<PathBuf>) -> u16 {
    let listener = test_listener();
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            listener.set_nonblocking(true).expect("nonblocking");
            let listener = tokio::net::TcpListener::from_std(listener).expect("from_std");
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                let gate = gate.clone();
                tokio::spawn(async move { serve(stream, handler, gate).await });
            }
        });
    });
    port
}

/// A free port nobody is listening on: the readiness probe must fail there.
pub fn dead_port() -> u16 {
    let listener = test_listener();
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

/// A readyz-only server that answers 200 while either marker file exists.
/// `lingering` belongs to `old`, a stand-in for the daemon that is already
/// running: it is removed `grace` after that process dies, so the port keeps
/// answering for a moment past the kill, like a real server that has not
/// finished shutting down. `fresh` is the one a replacement daemon creates.
pub fn marker_readyz(
    lingering: PathBuf,
    fresh: PathBuf,
    mut old: std::process::Child,
    grace: Duration,
) -> u16 {
    let listener = test_listener();
    let port = listener.local_addr().expect("addr").port();
    let doomed = lingering.clone();
    std::thread::spawn(move || {
        let _ = old.wait();
        std::thread::sleep(grace);
        let _ = std::fs::remove_file(&doomed);
    });
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            // Read the request before answering. Closing on an unread request
            // resets the connection, and the prober would read that as a
            // server that is not up rather than as the answer it just sent.
            let mut request = [0u8; 256];
            let _ = std::io::Read::read(&mut stream, &mut request);
            let status = if lingering.exists() || fresh.exists() {
                "200 OK"
            } else {
                "503 Service Unavailable"
            };
            let _ = std::io::Write::write_all(
                &mut stream,
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
    port
}

/// A readyz-only server that starts answering 200 `delay` after the call; the
/// listener is bound from the start, so earlier probes hang until they time
/// out. Keep `delay` above the CLI's probe timeout, so the first readiness
/// check fails and the CLI spawns the fake node first. The thread runs to the
/// end of the test process, like the mock server's.
pub fn delayed_readyz(delay: Duration) -> u16 {
    let listener = test_listener();
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    port
}

async fn serve(mut stream: tokio::net::TcpStream, handler: Handler, gate: Option<PathBuf>) {
    let mut probe = [0u8; 32];
    let mut seen = 0;
    for _ in 0..10 {
        match stream.peek(&mut probe).await {
            Ok(0) | Err(_) => return,
            Ok(n) => seen = n,
        }
        if seen >= 16 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if probe[..seen].starts_with(b"GET /readyz") {
        let up = match &gate {
            Some(path) => path.exists(),
            None => true,
        };
        let status = if up { "200 OK" } else { "503 Not Yet" };
        let _ = stream
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        let _ = stream.shutdown().await;
        return;
    }

    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            if sink.send(Message::Text(value.to_string())).await.is_err() {
                return;
            }
        }
    });
    while let Some(Ok(message)) = source.next().await {
        if let Message::Text(text) = message {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                handler(&value, &tx);
            }
        }
    }
}

pub fn ok(tx: &Tx, msg: &Value, result: Value) {
    let _ = tx.send(json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result }));
}

pub fn notify(tx: &Tx, method: &str, params: Value) {
    let _ = tx.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
}

pub fn server_request(tx: &Tx, id: impl serde::Serialize, method: &str, params: Value) {
    let _ = tx.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
}

pub fn thread_value(id: &str, status: Value, turns: Value) -> Value {
    json!({
        "id": id,
        "status": status,
        "cwd": "/tmp",
        "model": "mock-model",
        "turns": turns,
    })
}

pub fn idle() -> Value {
    json!({ "type": "idle" })
}

pub fn active() -> Value {
    json!({ "type": "active", "activeFlags": [] })
}

/// Answers the requests every scenario needs, and hands anything else to `extra`.
pub fn base_handler(
    recorder: Recorder,
    thread_id: &'static str,
    read_status: Value,
    extra: impl Fn(&Value, &Tx, &str) + Send + Sync + 'static,
) -> Handler {
    Arc::new(move |msg: &Value, tx: &Tx| {
        recorder.push(msg.clone());
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => ok(tx, msg, json!({ "userAgent": "mock/0" })),
            "initialized" => {}
            "thread/start" => ok(
                tx,
                msg,
                json!({ "thread": thread_value(thread_id, idle(), json!([])) }),
            ),
            "thread/read" => ok(
                tx,
                msg,
                json!({ "thread": thread_value(thread_id, read_status.clone(), json!([])) }),
            ),
            "thread/unsubscribe" => ok(tx, msg, json!({})),
            other => extra(msg, tx, other),
        }
    })
}

/// Configure a backend command with scratch-only paths and no inherited backend options.
pub fn isolated(backend: &str, port: u16, state: &TempDir) -> Command {
    assert!(
        ![12897, 12898, 12899].contains(&port),
        "reserved backend port"
    );
    let mut command = Command::new(BIN);
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("AGENT_BRIDGE_")
            || key_text.starts_with("CODEX_BRIDGE_")
            || key_text.starts_with("DSH_BRIDGE_")
        {
            command.env_remove(key);
        }
    }
    command
        .env_remove("HERDR_ENV")
        .env_remove("XDG_DATA_HOME")
        .env("AGENT_BRIDGE_STATE_DIR", state.path())
        .env("AGENT_BRIDGE_CONFIG", state.join("absent-config.toml"))
        .env("USERPROFILE", state.path())
        .env("HOME", state.path())
        .arg(backend)
        .arg("--url")
        .arg(format!("ws://127.0.0.1:{port}"));
    command
}

/// The Codex CLI, with a fake launcher even when readiness unexpectedly fails.
pub fn cli(port: u16, state: &TempDir) -> Command {
    let mut command = isolated("codex", port, state);
    command
        .env("AGENT_BRIDGE_CODEX_BIN", fake_codex(state))
        .env("AGENT_BRIDGE_CODEX_READY_TIMEOUT_MS", "1000")
        .env("FAKE_CODEX_LOG", state.join("codex-args.txt"));
    command
}

/// Put a node.cmd/node alias before the inherited PATH. Each script gets a distinct
/// directory so selecting a later fake cannot overwrite another running fixture.
pub fn node_path(script: &Path) -> std::ffi::OsString {
    let dir = script.with_extension("bin");
    std::fs::create_dir_all(&dir).expect("fake node PATH directory");
    let alias = dir.join(if cfg!(windows) { "node.cmd" } else { "node" });
    std::fs::copy(script, &alias).expect("fake node alias");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&alias, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&inherited)))
        .expect("fake node PATH")
}

#[allow(unused_imports)]
pub mod dsh {
    use super::*;
    pub use super::{
        code, dead_port, delayed_readyz, fake_late_web_node, fake_marker_node, fake_two_web_node,
        gated_mock, lines, marker_readyz, notify, ok, server_request, start_mock, Handler,
        Recorder, TempDir, Tx,
    };
    pub fn thread_value(id: &str, status: Value, turns: Value) -> Value {
        json!({
            "threadId": id,
            "status": status,
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "title": format!("dsh-{id}"),
            "model": "mock-model",
            "turns": turns,
            "pendingRequests": [],
        })
    }

    pub fn idle() -> Value {
        json!("idle")
    }

    pub fn active() -> Value {
        json!("running")
    }

    /// Answers the requests every scenario needs, and hands anything else to `extra`.
    pub fn base_handler(
        recorder: Recorder,
        thread_id: &'static str,
        read_status: Value,
        extra: impl Fn(&Value, &Tx, &str) + Send + Sync + 'static,
    ) -> Handler {
        Arc::new(move |msg: &Value, tx: &Tx| {
            recorder.push(msg.clone());
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            match method {
                "initialize" => ok(
                    tx,
                    msg,
                    json!({
                        "serverVersion": "0.1.0",
                        "dshVersion": "mock/0",
                        "uiUrl": "http://127.0.0.1:18080/",
                        "profile": "bridge",
                    }),
                ),
                "thread/start" => ok(
                    tx,
                    msg,
                    json!({
                        "threadId": thread_id,
                        "title": format!("dsh-{thread_id}"),
                        "cwd": env!("CARGO_MANIFEST_DIR"),
                    }),
                ),
                "thread/read" => ok(
                    tx,
                    msg,
                    thread_value(thread_id, read_status.clone(), json!([])),
                ),
                other => extra(msg, tx, other),
            }
        })
    }

    /// Select a fake Node and a scratch entry point so PATH never starts a real dsh.
    pub fn cli(port: u16, state: &TempDir) -> Command {
        let mut command = super::isolated("dsh", port, state);
        command
            .env("AGENT_BRIDGE_DSH_NODE_BIN", super::fake_node(state))
            .env("AGENT_BRIDGE_DSH_BIN", state.join("mock dsh bin.js"))
            .env("AGENT_BRIDGE_DSH_READY_TIMEOUT_MS", "15000")
            .env("FAKE_NODE_LOG", state.join("node-args.txt"));
        command
    }
}

pub fn lines(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
        .collect()
}

pub fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}
