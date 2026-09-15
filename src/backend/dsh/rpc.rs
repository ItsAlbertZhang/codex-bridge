//! dsh framing: string request ids, strict envelopes and business diagnostics.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::super::rpc::{Protocol, RpcError};

pub struct Wire;

impl Protocol for Wire {
    fn next_id(&self, sequence: i64) -> Value {
        json!(format!("client-{sequence}"))
    }

    fn response_key(&self, id: &Value) -> Option<String> {
        id.as_str().map(str::to_string)
    }

    fn decode(&self, text: &str) -> Result<Value> {
        let msg: Value = serde_json::from_str(text).context("invalid JSON from server")?;
        if !msg.is_object() || msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            bail!("expected a JSON-RPC 2.0 object");
        }
        if msg.get("id").is_some_and(|id| !id.is_string()) {
            bail!("JSON-RPC id must be a string");
        }
        if msg.get("method").is_some_and(|method| !method.is_string()) {
            bail!("JSON-RPC method must be a string");
        }
        if msg.get("id").is_some()
            && msg.get("method").is_none()
            && msg.get("result").is_some() == msg.get("error").is_some()
        {
            bail!("JSON-RPC response must contain exactly one of result or error");
        }
        if msg.get("id").is_none() && msg.get("method").is_none() {
            bail!("JSON-RPC message has neither method nor id");
        }
        Ok(msg)
    }

    fn binary(&self, _bytes: &[u8]) -> Result<Value> {
        bail!("expected a JSON text frame")
    }

    fn describe_error(&self, error: &RpcError) -> String {
        let kind = error
            .data
            .as_ref()
            .and_then(|data| data.get("kind"))
            .and_then(Value::as_str);
        let description = match kind {
            Some("thread_not_found") => "thread not found",
            Some("thread_busy") => "thread is already running",
            Some("no_active_turn") => "thread has no active turn",
            Some("turn_mismatch") => "expected turn does not match the active turn",
            Some("invalid_cwd") => "invalid working directory",
            Some("not_resumable") => "thread cannot be resumed",
            Some("internal") => "internal server error",
            Some(other) => other,
            None => "rpc error",
        };
        let mut description = format!("{description} ({}): {}", error.code, error.message);
        if let Some(data) = &error.data {
            description.push_str(&format!(" ({data})"));
        }
        description
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_envelopes_fail_instead_of_leaving_a_request_waiting() {
        for raw in [
            "{",
            "[]",
            r#"{"jsonrpc":"1.0","id":"client-1","result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":"client-1"}"#,
            r#"{"jsonrpc":"2.0","id":"client-1","result":{},"error":{}}"#,
        ] {
            assert!(Wire.decode(raw).is_err(), "{raw}");
        }
    }
}
