//! Token-bearing dsh browser URLs from complete daemon log lines.

use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use tokio::time::Instant;

const LOG_FILE: &str = "daemon.log";
const DEFAULT_UI_URL_WAIT_MS: u64 = 5_000;
const UI_URL_POLL_MS: u64 = 200;

pub(super) fn setting(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub(super) fn ui_port() -> String {
    setting("AGENT_BRIDGE_DSH_UI_PORT", "12899")
}

fn log_path() -> std::path::PathBuf {
    crate::core::daemon::state_dir(&super::Dsh).join(LOG_FILE)
}

pub(super) fn logged_ui_url() -> Option<String> {
    let log = std::fs::read_to_string(log_path()).ok()?;
    last_complete_ui_url(&log)
}

pub(super) fn ui_url() -> String {
    logged_ui_url().unwrap_or_else(|| format!("http://127.0.0.1:{}/", ui_port()))
}

/// Ignore a URL whose line is still being written by the daemon.
fn complete_lines(text: &str) -> &str {
    match text.rfind('\n') {
        Some(end) => &text[..=end],
        None => "",
    }
}

fn last_complete_ui_url(log: &str) -> Option<String> {
    ui_url_matches(complete_lines(log)).last()
}

fn first_complete_ui_url(tail: &str) -> Option<String> {
    ui_url_matches(complete_lines(tail)).next()
}

fn ui_url_matches(log: &str) -> impl Iterator<Item = String> + '_ {
    const PREFIX: &str = "dsh web: ";
    log.match_indices(PREFIX).filter_map(move |(index, _)| {
        let rest = &log[index + PREFIX.len()..];
        let url = rest.split_whitespace().next()?;
        (rest.starts_with("http://") && url.len() > "http://".len()).then(|| url.to_string())
    })
}

fn ui_url_wait_ms() -> u64 {
    std::env::var("AGENT_BRIDGE_DSH_UI_URL_WAIT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_UI_URL_WAIT_MS)
}

fn fresh_ui_url(offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(log_path()).ok()?;
    if file.metadata().ok()?.len() <= offset {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    first_complete_ui_url(&String::from_utf8_lossy(&tail))
}

/// Only explicit daemon start/restart waits for the URL from this spawn.
pub(super) async fn wait_for_new_ui_url(offset: u64) -> String {
    let budget = ui_url_wait_ms();
    let deadline = Instant::now() + Duration::from_millis(budget);
    loop {
        if let Some(url) = fresh_ui_url(offset) {
            return url;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "no new `dsh web:` line in {} within {budget} ms; using the last one",
                log_path().display()
            );
            return ui_url();
        }
        tokio::time::sleep(Duration::from_millis(UI_URL_POLL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ui_url_scan_uses_the_last_exact_http_match() {
        assert_eq!(
            last_complete_ui_url(
                "dsh web: http://localhost:7/?token=old\r\n\
             dsh web: http://localhost:8/?token=new trailing\n\
             dsh web: https://ignored/\n\
             dsh web:  http://ignored/\n\
             dsh web: http://\n"
            ),
            Some("http://localhost:8/?token=new".to_string())
        );
        assert_eq!(last_complete_ui_url("ordinary startup output"), None);
    }

    #[test]
    fn the_fresh_ui_url_scan_reads_the_first_complete_line_only() {
        assert_eq!(
            first_complete_ui_url("dsh web: http://localhost:7/?token=half"),
            None
        );
        assert_eq!(
            first_complete_ui_url(
                "dsh web: http://localhost:7/?token=first\n\
             dsh web: http://localhost:8/?token=second\n"
            ),
            Some("http://localhost:7/?token=first".to_string())
        );
        assert_eq!(
            first_complete_ui_url(
                "dsh web: http://localhost:7/?token=first\r\n\
             dsh web: http://localhost:8/?token=half"
            ),
            Some("http://localhost:7/?token=first".to_string())
        );
        assert_eq!(first_complete_ui_url("ordinary startup output\n"), None);
    }
}
