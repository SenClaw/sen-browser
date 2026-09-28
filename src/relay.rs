//! The extension driver: tabs in the person's own Chrome, reached through the
//! SenClaw extension's `chrome.debugger` and relayed by the daemon.
//!
//! The daemon owns the extension's connection (pairing, Origin, token) and
//! opens `/v1/drivers/extension` on this runtime only while a task needs it,
//! forwarding `drv` frames verbatim in both directions:
//!
//! ```text
//! runtime → ext   {"ch":"drv","id":7,"t":"open_tab","url":"…","owner":"…"}
//! ext → runtime   {"ch":"drv","id":7,"ok":{"tab":1873}}  |  {"ch":"drv","id":7,"err":"blocked_method","message":"…"}
//! ext → runtime   {"ch":"drv","t":"event","tab":1873,"method":"Page.loadEventFired","params":{…}}
//!                 {"ch":"drv","t":"detached","tab":1873,"reason":"canceled_by_user"}
//!                 {"ch":"drv","t":"tab_closed","tab":1873}  {"ch":"drv","t":"user_active","tab":1873}
//!                 {"ch":"drv","t":"shared_tab","tab":2210,"url":"…","title":"…"}
//!                 {"ch":"drv","t":"hello","ext":"…","v":"0.2.0","scripts":"sha256:…","chrome":"154.0…"}
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::cdp::{CdpError, CdpEvent, TabChannel, COMMAND_TIMEOUT};

pub struct RelayConn {
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>,
    next_id: AtomicU64,
    events: broadcast::Sender<CdpEvent>,
    hello: Mutex<Option<Value>>,
    shared: Mutex<Vec<Value>>,
    closed: AtomicBool,
}

impl RelayConn {
    pub fn new(tx: mpsc::UnboundedSender<String>) -> Arc<RelayConn> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(RelayConn {
            tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
            hello: Mutex::new(None),
            shared: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub async fn hello(&self) -> Option<Value> {
        self.hello.lock().await.clone()
    }

    /// Tabs the person shared from the side panel, not yet adopted.
    pub async fn shared_tabs(&self) -> Vec<Value> {
        self.shared.lock().await.clone()
    }

    pub async fn take_shared(&self, tab: i64) -> Option<Value> {
        let mut shared = self.shared.lock().await;
        let pos = shared.iter().position(|s| s.get("tab").and_then(Value::as_i64) == Some(tab))?;
        Some(shared.remove(pos))
    }

    /// One frame from the extension (already stripped of nothing: the daemon
    /// forwards it verbatim).
    pub async fn handle_frame(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        if v.get("ch").and_then(Value::as_str) != Some("drv") {
            return;
        }
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            if v.get("ok").is_some() || v.get("err").is_some() {
                if let Some(waiter) = self.pending.lock().await.remove(&id) {
                    let answer = match v.get("err").and_then(Value::as_str) {
                        None => Ok(v.get("ok").cloned().unwrap_or(Value::Null)),
                        Some(code) => {
                            let message = v.get("message").and_then(Value::as_str).unwrap_or(code).to_string();
                            if code == "blocked_method" || code == "blocked_script" || code == "not_owned" {
                                Err(CdpError::Blocked(message))
                            } else {
                                Err(CdpError::Protocol { method: String::new(), code: 0, message })
                            }
                        }
                    };
                    let _ = waiter.send(answer);
                }
                return;
            }
        }
        let tab = v.get("tab").and_then(Value::as_i64);
        let internal = |method: &str| CdpEvent { method: method.to_string(), params: v.clone(), session: None, tab };
        match v.get("t").and_then(Value::as_str) {
            Some("event") => {
                let _ = self.events.send(CdpEvent {
                    method: v.get("method").and_then(Value::as_str).unwrap_or_default().to_string(),
                    params: v.get("params").cloned().unwrap_or(Value::Null),
                    session: v.get("session").and_then(Value::as_str).map(str::to_string),
                    tab,
                });
            }
            Some("detached") => {
                let _ = self.events.send(internal("Sen.detached"));
            }
            Some("tab_closed") => {
                let _ = self.events.send(internal("Sen.tabClosed"));
            }
            Some("user_active") => {
                let _ = self.events.send(internal("Sen.userActive"));
            }
            Some("shared_tab") => {
                let mut shared = self.shared.lock().await;
                shared.retain(|s| s.get("tab").and_then(Value::as_i64) != tab);
                shared.push(json!({ "tab": tab, "url": v.get("url"), "title": v.get("title") }));
            }
            Some("hello") => {
                *self.hello.lock().await = Some(v.clone());
            }
            _ => {}
        }
    }

    /// Send one request to the extension and wait for its answer.
    pub async fn request(&self, t: &str, fields: Value) -> Result<Value, CdpError> {
        if self.is_closed() {
            return Err(CdpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut frame = json!({ "ch": "drv", "id": id, "t": t });
        if let (Some(obj), Some(extra)) = (frame.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.tx.send(frame.to_string()).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(CdpError::Closed);
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(CdpError::Timeout(t.to_string()))
            }
        }
    }

    /// A frame that expects no answer (for example rejecting the extension).
    pub fn notify(&self, t: &str, fields: Value) {
        let mut frame = json!({ "ch": "drv", "t": t });
        if let (Some(obj), Some(extra)) = (frame.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        let _ = self.tx.send(frame.to_string());
    }

    /// The connection is gone: fail every waiter and tell every tab.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for (_, waiter) in self.pending.lock().await.drain() {
            let _ = waiter.send(Err(CdpError::Closed));
        }
        let _ = self.events.send(CdpEvent { method: "Sen.connectionClosed".into(), params: Value::Null, session: None, tab: None });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }
}

/// A tab in the person's Chrome.
pub struct ExtTabChannel {
    pub relay: Arc<RelayConn>,
    pub tab: i64,
}

#[async_trait]
impl TabChannel for ExtTabChannel {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.relay
            .request("cdp", json!({ "tab": self.tab, "session": Value::Null, "method": method, "params": params }))
            .await
    }

    fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.relay.subscribe()
    }

    fn owns(&self, event: &CdpEvent) -> bool {
        event.tab == Some(self.tab) || event.method == "Sen.connectionClosed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_resolve_requests_and_errors_map_to_blocked() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let relay = RelayConn::new(tx);
        let r = relay.clone();
        let waiter = tokio::spawn(async move { r.request("open_tab", json!({ "url": "https://a.test" })).await });
        let sent: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(sent["t"], "open_tab");
        assert_eq!(sent["url"], "https://a.test");
        let id = sent["id"].as_u64().unwrap();
        relay.handle_frame(&json!({ "ch": "drv", "id": id, "ok": { "tab": 42 } }).to_string()).await;
        assert_eq!(waiter.await.unwrap().unwrap(), json!({ "tab": 42 }));

        let r = relay.clone();
        let waiter = tokio::spawn(async move { r.request("cdp", json!({ "tab": 42, "method": "Network.getCookies" })).await });
        let sent: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let id = sent["id"].as_u64().unwrap();
        relay
            .handle_frame(&json!({ "ch": "drv", "id": id, "err": "blocked_method", "message": "Network.getCookies" }).to_string())
            .await;
        assert!(matches!(waiter.await.unwrap(), Err(CdpError::Blocked(_))));
    }

    #[tokio::test]
    async fn notices_become_tab_events_and_close_fails_waiters() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let relay = RelayConn::new(tx);
        let mut events = relay.subscribe();
        relay.handle_frame(&json!({ "ch": "drv", "t": "detached", "tab": 5, "reason": "canceled_by_user" }).to_string()).await;
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.method, "Sen.detached");
        assert_eq!(ev.tab, Some(5));
        relay.handle_frame(&json!({ "ch": "drv", "t": "shared_tab", "tab": 9, "url": "https://b.test" }).to_string()).await;
        assert_eq!(relay.shared_tabs().await.len(), 1);
        assert!(relay.take_shared(9).await.is_some());

        let r = relay.clone();
        let waiter = tokio::spawn(async move { r.request("attach", json!({ "tab": 5 })).await });
        tokio::task::yield_now().await;
        relay.close().await;
        assert!(matches!(waiter.await.unwrap(), Err(CdpError::Closed)));
    }
}
