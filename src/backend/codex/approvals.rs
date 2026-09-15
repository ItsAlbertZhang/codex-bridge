//! Decision -> response-body mapping for the server requests the CLI can answer.
//!
//! The app-server treats an unknown `decision` value as a failed approval
//! without complaining, so every value is validated here before it is sent.
//! Shapes come from `docs/codex-schema/*Response.json`.

use anyhow::{bail, Result};
use serde_json::{json, Value};

pub const COMMAND_EXECUTION: &str = "item/commandExecution/requestApproval";
pub const FILE_CHANGE: &str = "item/fileChange/requestApproval";
pub const TOOL_USER_INPUT: &str = "item/tool/requestUserInput";
pub const MCP_ELICITATION: &str = "mcpServer/elicitation/request";
pub const PERMISSIONS: &str = "item/permissions/requestApproval";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Decision {
    Accept,
    AcceptForSession,
    Decline,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Accept => "accept",
            Decision::AcceptForSession => "acceptForSession",
            Decision::Decline => "decline",
        }
    }
}

/// Build the response body for `method` from a `--decision` value.
///
/// Requests whose response carries structured payload (answers, granted
/// permissions) have no sensible mapping from a bare decision; they must be
/// answered with `--result-json`.
pub fn response_for(method: &str, decision: Decision) -> Result<Value> {
    match method {
        COMMAND_EXECUTION | FILE_CHANGE => Ok(json!({ "decision": decision.as_str() })),
        MCP_ELICITATION => match decision {
            Decision::AcceptForSession => bail!(
                "{MCP_ELICITATION} accepts only accept/decline/cancel; \
                 use --result-json for anything else"
            ),
            _ => Ok(json!({ "action": decision.as_str() })),
        },
        TOOL_USER_INPUT => bail!(
            "{TOOL_USER_INPUT} needs answers; reply with \
             --result-json '{{\"answers\":{{\"<questionId>\":{{\"answers\":[\"...\"]}}}}}}'"
        ),
        PERMISSIONS => bail!(
            "{PERMISSIONS} needs a granted permission profile; reply with \
             --result-json '{{\"permissions\":{{...}},\"scope\":\"turn\"}}'"
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
    fn approval_decisions_match_the_schema() {
        assert_eq!(
            response_for(COMMAND_EXECUTION, Decision::Accept).unwrap(),
            json!({ "decision": "accept" })
        );
        assert_eq!(
            response_for(FILE_CHANGE, Decision::AcceptForSession).unwrap(),
            json!({ "decision": "acceptForSession" })
        );
        assert_eq!(
            response_for(COMMAND_EXECUTION, Decision::Decline).unwrap(),
            json!({ "decision": "decline" })
        );
        assert_eq!(
            response_for(MCP_ELICITATION, Decision::Decline).unwrap(),
            json!({ "action": "decline" })
        );
    }

    #[test]
    fn structured_responses_refuse_a_bare_decision() {
        for method in [TOOL_USER_INPUT, PERMISSIONS, "item/tool/call"] {
            assert!(response_for(method, Decision::Accept).is_err(), "{method}");
        }
        assert!(response_for(MCP_ELICITATION, Decision::AcceptForSession).is_err());
    }
}
