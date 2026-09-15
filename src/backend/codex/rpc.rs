//! Codex framing: numeric client ids and the app-server's permissive reader.

use anyhow::Result;
use serde_json::{json, Value};

use super::super::rpc::{Protocol, RpcError};

pub struct Wire;

impl Protocol for Wire {
    fn next_id(&self, sequence: i64) -> Value {
        json!(sequence)
    }

    fn response_key(&self, id: &Value) -> Option<String> {
        id.as_i64().map(|id| id.to_string())
    }

    fn decode(&self, text: &str) -> Result<Value> {
        Ok(serde_json::from_str(text)
            .unwrap_or_else(|_| json!({"method":"$/unparseable","params":{"raw":text}})))
    }

    fn binary(&self, bytes: &[u8]) -> Result<Value> {
        self.decode(&String::from_utf8_lossy(bytes))
    }

    fn describe_error(&self, error: &RpcError) -> String {
        let mut description = format!("rpc error {}: {}", error.code, error.message);
        if let Some(data) = &error.data {
            description.push_str(&format!(" ({data})"));
        }
        description
    }
}
