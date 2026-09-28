//! Chrome DevTools Protocol plumbing.
//!
//! [`WsConnection`] is one JSON-RPC WebSocket to a browser we launched: commands
//! carry an `id` and an optional flat `sessionId`, answers resolve the waiting
//! caller, and everything else is an event broadcast to whoever subscribed.
//! [`TabChannel`] is what the rest of the runtime talks to: "send this command
//! to *my* tab, and show me *my* tab's events" — implemented once over a flat
//! session ([`SessionChannel`]) and once over the extension relay
//! ([`crate::relay::ExtTabChannel`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// Longest any single command may take. Navigation and screenshots of a heavy
/// page are the slow cases; anything past this is a wedged renderer.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, thiserror::Error)]
pub enum CdpError {
    #[error("the CDP connection closed")]
    Closed,
    #[error("CDP {method} failed ({code}): {message}")]
    Protocol { method: String, code: i64, message: String },
    #[error("CDP {0} timed out")]
    Timeout(String),
    #[error("CDP method {0} is not allowed")]
    Blocked(String),
    #[error("{0}")]
    Other(String),
}

impl CdpError {
    /// The execution context the call targeted is gone (navigation, reload).
    pub fn is_context_lost(&self) -> bool {
        matches!(self, CdpError::Protocol { message, .. }
            if message.contains("Cannot find context") || message.contains("context was destroyed")
                || message.contains("Execution context was destroyed") || message.contains("Inspected target navigated"))
    }
}

/// One protocol event. `session` is the flat session id (managed driver);
/// `tab` is the extension's tab id (relay driver).
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
    pub tab: Option<i64>,
}

/// A command channel to exactly one tab, whatever carries it.
#[async_trait]
pub trait TabChannel: Send + Sync {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CdpError>;
    /// Every event the underlying connection sees; filter with [`TabChannel::owns`].
    fn subscribe(&self) -> broadcast::Receiver<CdpEvent>;
    /// Whether `event` belongs to this tab.
    fn owns(&self, event: &CdpEvent) -> bool;
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>;

/// A browser-level CDP WebSocket.
pub struct WsConnection {
    tx: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<CdpEvent>,
    closed: Arc<AtomicBool>,
}

impl WsConnection {
    pub async fn connect(url: &str) -> Result<Arc<WsConnection>, CdpError> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| CdpError::Other(format!("could not connect to {url}: {e}")))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);
        let closed = Arc::new(AtomicBool::new(false));

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        {
            let pending = pending.clone();
            let events = events.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    if let Some(id) = v.get("id").and_then(Value::as_u64) {
                        if let Some(waiter) = pending.lock().await.remove(&id) {
                            let _ = waiter.send(answer_of(&v));
                        }
                    } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                        let _ = events.send(CdpEvent {
                            method: method.to_string(),
                            params: v.get("params").cloned().unwrap_or(Value::Null),
                            session: v.get("sessionId").and_then(Value::as_str).map(str::to_string),
                            tab: None,
                        });
                    }
                }
                closed.store(true, Ordering::SeqCst);
                for (_, waiter) in pending.lock().await.drain() {
                    let _ = waiter.send(Err(CdpError::Closed));
                }
                let _ = events.send(CdpEvent {
                    method: "Sen.connectionClosed".into(),
                    params: Value::Null,
                    session: None,
                    tab: None,
                });
            });
        }

        Ok(Arc::new(WsConnection { tx, pending, next_id: AtomicU64::new(1), events, closed }))
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send one command and wait for its answer.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, CdpError> {
        if self.is_closed() {
            return Err(CdpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.tx.send(Message::Text(msg.to_string())).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(CdpError::Closed);
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer.map_err(|e| with_method(e, method)),
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(CdpError::Timeout(method.to_string()))
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }
}

/// `{"result": …}` or `{"error": {code, message}}` → a Rust result.
pub fn answer_of(v: &Value) -> Result<Value, CdpError> {
    if let Some(err) = v.get("error") {
        return Err(CdpError::Protocol {
            method: String::new(),
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
        });
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

fn with_method(e: CdpError, method: &str) -> CdpError {
    match e {
        CdpError::Protocol { code, message, .. } => CdpError::Protocol { method: method.to_string(), code, message },
        other => other,
    }
}

/// A tab reached through a flat session on a [`WsConnection`].
pub struct SessionChannel {
    pub conn: Arc<WsConnection>,
    pub session: String,
}

#[async_trait]
impl TabChannel for SessionChannel {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.conn.call(method, params, Some(&self.session)).await
    }

    fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.conn.subscribe()
    }

    fn owns(&self, event: &CdpEvent) -> bool {
        event.session.as_deref() == Some(self.session.as_str()) || event.method == "Sen.connectionClosed"
    }
}
