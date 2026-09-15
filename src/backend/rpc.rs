//! WebSocket transport, request pairing, and notification fan-out.
//!
//! Framing, identifiers and error descriptions are supplied by each protocol.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<WsStream, Message>;
type Pending = Arc<StdMutex<HashMap<String, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Debug)]
pub enum Event {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Closed {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// The transport has no knowledge of a backend name or its wire conventions.
pub trait Protocol: Send + Sync {
    fn next_id(&self, sequence: i64) -> Value;
    fn response_key(&self, id: &Value) -> Option<String>;
    fn decode(&self, text: &str) -> Result<Value>;
    fn binary(&self, bytes: &[u8]) -> Result<Value>;
    fn describe_error(&self, error: &RpcError) -> String;
}

pub struct Connection {
    sink: Mutex<Sink>,
    pending: Pending,
    next_id: AtomicI64,
    protocol: &'static dyn Protocol,
}

pub async fn connect(
    url: &str,
    protocol: &'static dyn Protocol,
) -> Result<(Arc<Connection>, mpsc::UnboundedReceiver<Event>)> {
    let (stream, _) = connect_async(url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let (sink, mut source) = stream.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
    let conn = Arc::new(Connection {
        sink: Mutex::new(sink),
        pending: Arc::clone(&pending),
        next_id: AtomicI64::new(1),
        protocol,
    });

    tokio::spawn(async move {
        let reason = loop {
            let decoded = match source.next().await {
                Some(Ok(Message::Text(text))) => protocol.decode(&text),
                Some(Ok(Message::Binary(bytes))) => protocol.binary(&bytes),
                Some(Ok(Message::Close(_))) => break "server closed the connection".to_string(),
                Some(Ok(_)) => continue,
                Some(Err(err)) => break err.to_string(),
                None => break "connection ended".to_string(),
            };
            match decoded {
                Ok(msg) => dispatch(msg, &pending, &tx, protocol),
                Err(err) => break format!("protocol error: {err:#}"),
            }
        };
        for (_, waiter) in pending.lock().expect("pending map poisoned").drain() {
            let _ = waiter.send(Err(RpcError {
                code: -32000,
                message: format!("connection closed: {reason}"),
                data: None,
            }));
        }
        let _ = tx.send(Event::Closed { reason });
    });
    Ok((conn, rx))
}

fn dispatch(
    msg: Value,
    pending: &Pending,
    tx: &mpsc::UnboundedSender<Event>,
    protocol: &dyn Protocol,
) {
    let id = msg.get("id").cloned();
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (id, method) {
        (Some(id), Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let _ = tx.send(Event::ServerRequest { id, method, params });
        }
        (Some(id), None) => {
            let Some(key) = protocol.response_key(&id) else {
                return;
            };
            let Some(waiter) = pending.lock().expect("pending map poisoned").remove(&key) else {
                return;
            };
            let outcome = match msg.get("error") {
                Some(err) => Err(RpcError {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                    data: err.get("data").cloned(),
                }),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(outcome);
        }
        (None, Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let _ = tx.send(Event::Notification { method, params });
        }
        (None, None) => {}
    }
}

impl Connection {
    async fn send(&self, value: Value) -> Result<()> {
        self.sink
            .lock()
            .await
            .send(Message::Text(serde_json::to_string(&value)?))
            .await
            .context("writing to the websocket")
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .protocol
            .next_id(self.next_id.fetch_add(1, Ordering::SeqCst));
        let key = self
            .protocol
            .response_key(&id)
            .expect("allocated request id has a response key");
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending map poisoned")
            .insert(key, tx);
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(err)) => Err(anyhow!(
                "{method} failed: {}",
                self.protocol.describe_error(&err)
            )),
            Err(_) => Err(anyhow!("connection closed while waiting for {method}")),
        }
    }

    /// Put a request on the wire without retaining its response.
    pub async fn request_no_reply(&self, method: &str, params: Value) -> Result<()> {
        let id = self
            .protocol
            .next_id(self.next_id.fetch_add(1, Ordering::SeqCst));
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut msg = json!({"jsonrpc":"2.0","method":method});
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send(msg).await
    }

    pub async fn respond_result(&self, id: &Value, result: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }

    pub async fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_response_ids_pair_independently_of_server_requests() {
        let protocol = super::super::dsh::rpc::Wire;
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = oneshot::channel();
        pending
            .lock()
            .unwrap()
            .insert("client-1".into(), response_tx);
        dispatch(
            json!({"jsonrpc":"2.0","id":"req-1","method":"approval/request","params":{"threadId":"T1"}}),
            &pending,
            &event_tx,
            &protocol,
        );
        assert!(
            matches!(event_rx.try_recv().unwrap(), Event::ServerRequest { id, .. } if id == "req-1")
        );
        assert_eq!(pending.lock().unwrap().len(), 1);
        dispatch(
            json!({"jsonrpc":"2.0","id":"client-1","result":{"threadId":"T1"}}),
            &pending,
            &event_tx,
            &protocol,
        );
        assert_eq!(
            response_rx.try_recv().unwrap().unwrap(),
            json!({"threadId":"T1"})
        );
        assert!(pending.lock().unwrap().is_empty());
    }
}
