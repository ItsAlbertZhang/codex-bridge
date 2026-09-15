//! JSON event output and per-thread append-only logging.
use super::{daemon, output};
use crate::backend::Backend;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};
/// stdout carries JSON only, one object per line; the optional log file gets the
/// same lines plus the per-item summaries.
pub(super) struct Emitter {
    log: Option<std::fs::File>,
    pub(super) log_path: Option<PathBuf>,
    pub(super) unlogged_events: usize,
    pub(super) suppress_log: bool,
}

impl Emitter {
    pub(super) fn new(path: Option<&Path>) -> Result<Self> {
        let log = match path {
            Some(path) => Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("opening log file {}", path.display()))?,
            ),
            None => None,
        };
        Ok(Self {
            log,
            log_path: path.map(Path::to_path_buf),
            unlogged_events: 0,
            suppress_log: false,
        })
    }

    pub(super) fn for_thread(backend: &impl Backend, thread_id: &str) -> Result<Self> {
        let mut components = Path::new(thread_id).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            bail!("invalid thread id for a log filename: {thread_id}");
        }
        let dir = daemon::state_dir(backend).join("logs");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating log directory {}", dir.display()))?;
        Self::new(Some(&dir.join(format!("{thread_id}.jsonl"))))
    }

    /// stdout + log.
    pub(super) fn emit(&mut self, value: &Value) {
        let mut value = value.clone();
        if matches!(
            value.get("event").and_then(Value::as_str),
            Some("started" | "turn")
        ) {
            if let Some(path) = &self.log_path {
                value["logPath"] = json!(path);
            }
        }
        let line = value.to_string();
        output::write_line(&line);
        self.write_log(&line);
    }

    /// log only (item summaries, status changes, auto-declines).
    pub(super) fn log(&mut self, value: &Value) {
        let line = value.to_string();
        self.write_log(&line);
    }

    fn write_log(&mut self, line: &str) {
        if self.suppress_log {
            return;
        }
        if let Some(file) = self.log.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}
