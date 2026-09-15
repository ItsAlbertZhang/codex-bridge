//! Decision -> response-body mapping for the server requests the CLI can answer.
//!
//! The plugin only accepts one-time approval decisions. Shapes follow the
//! "Server-initiated requests" section of dsh-plugin/README.md.

use anyhow::{bail, Result};
use serde_json::{json, Value};

pub const APPROVAL: &str = "approval/request";
pub const USER_QUESTION: &str = "userQuestion/request";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Decision {
    Accept,
    Decline,
}

/// Build the response body for a one-time approval. Questions need a structured
/// answer and must be answered with `--result-json`.
pub fn response_for(method: &str, decision: Decision) -> Result<Value> {
    match method {
        APPROVAL => Ok(json!({ "decision": match decision {
            Decision::Accept => "allowed-once",
            Decision::Decline => "rejected",
        } })),
        USER_QUESTION => bail!(
            "{USER_QUESTION} needs answers; reply with --result-json containing an answers array"
        ),
        other => {
            bail!("no --decision mapping for server request method `{other}`; use --result-json")
        }
    }
}

/// The error body sent for every server request under `--auto-decline`.
pub const UNATTENDED_MESSAGE: &str =
    "agent-bridge is running this turn unattended: no human is available to approve or answer. \
     Use your own judgment and continue with what you can do without this approval.";

pub const UNATTENDED_CODE: i64 = -32000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_decisions_match_the_protocol() {
        assert_eq!(
            response_for(APPROVAL, Decision::Accept).unwrap(),
            json!({"decision":"allowed-once"})
        );
        assert_eq!(
            response_for(APPROVAL, Decision::Decline).unwrap(),
            json!({"decision":"rejected"})
        );
    }

    #[test]
    fn structured_responses_refuse_a_bare_decision() {
        for method in [USER_QUESTION, "unknown/request"] {
            assert!(response_for(method, Decision::Accept).is_err(), "{method}");
        }
    }
}
