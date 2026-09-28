//! One tab the agent owns: observe it in one call, act on it with trusted
//! input only after proving the page is still the one observed, and keep the
//! bookkeeping (isolated world, dialogs, handover) straight.
//!
//! The same code runs for both drivers; only the [`TabChannel`] differs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::allowlist;
use crate::cdp::{CdpError, TabChannel};
use crate::error::{BrowserError, Result};
use crate::observation::{self, Stored, DEFAULT_MAX_CANDIDATES, DEFAULT_TEXT_CHARS};
use crate::scripts::{self, Script};

/// How long an observation may wait for a navigating page to produce a body.
const OBSERVE_DEADLINE: Duration = Duration::from_secs(10);
/// Settle waits are capped at 200 ms inside the page; this bounds a wedged one.
const SETTLE_DEADLINE: Duration = Duration::from_millis(1500);
/// Navigation: wait this long for the load event before observing anyway.
const LOAD_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    Managed,
    Extension,
}

/// Why the agent may not use a tab right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    None,
    /// Handed to the person (login, one-time code, CAPTCHA).
    Handover,
    /// The person clicked into the tab; the agent yields.
    UserActive,
    /// The person cancelled the debugger session from Chrome's bar.
    Detached,
}

#[derive(Debug)]
enum CallError {
    /// The page threw, or the document went away under the call.
    Page(String),
    Cdp(CdpError),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Page(m) => write!(f, "{m}"),
            CallError::Cdp(e) => write!(f, "{e}"),
        }
    }
}

pub struct Tab {
    pub id: String,
    pub owner: Option<String>,
    pub driver: DriverKind,
    /// Managed: the CDP target id. Extension: the Chrome tab id.
    target: RwLock<String>,
    channel: RwLock<Arc<dyn TabChannel>>,
    world: Arc<Mutex<Option<i64>>>,
    dialog: Arc<Mutex<Option<Value>>>,
    last_url: Arc<Mutex<String>>,
    hold: Arc<Mutex<Hold>>,
    closed: Arc<AtomicBool>,
    last: Mutex<Option<Stored>>,
    seq: AtomicU64,
    /// One observe/act at a time per tab.
    op: Mutex<()>,
    listener: Mutex<Option<JoinHandle<()>>>,
    emulate_viewport: bool,
}

impl Tab {
    /// Wrap a freshly opened or attached tab and prepare it (domains, focus
    /// emulation so a background tab keeps rendering, viewport for managed).
    pub async fn attach(
        id: String,
        owner: Option<String>,
        driver: DriverKind,
        target: String,
        channel: Arc<dyn TabChannel>,
        emulate_viewport: bool,
    ) -> Result<Arc<Tab>> {
        let tab = Arc::new(Tab {
            id,
            owner,
            driver,
            target: RwLock::new(target),
            channel: RwLock::new(channel),
            world: Arc::new(Mutex::new(None)),
            dialog: Arc::new(Mutex::new(None)),
            last_url: Arc::new(Mutex::new(String::new())),
            hold: Arc::new(Mutex::new(Hold::None)),
            closed: Arc::new(AtomicBool::new(false)),
            last: Mutex::new(None),
            seq: AtomicU64::new(0),
            op: Mutex::new(()),
            listener: Mutex::new(None),
            emulate_viewport,
        });
        tab.start_listener().await;
        tab.setup().await?;
        Ok(tab)
    }

    pub async fn target(&self) -> String {
        self.target.read().await.clone()
    }

    pub async fn last_url(&self) -> String {
        self.last_url.lock().await.clone()
    }

    pub async fn hold(&self) -> Hold {
        *self.hold.lock().await
    }

    pub async fn set_hold(&self, hold: Hold) {
        *self.hold.lock().await = hold;
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Swap the channel after a relaunch (managed handover) or a re-attach
    /// (extension), and prepare the tab again. Node identities do not survive.
    pub async fn rebind(&self, target: String, channel: Arc<dyn TabChannel>) -> Result<()> {
        if let Some(h) = self.listener.lock().await.take() {
            h.abort();
        }
        *self.target.write().await = target;
        *self.channel.write().await = channel;
        *self.world.lock().await = None;
        *self.last.lock().await = None;
        *self.dialog.lock().await = None;
        self.closed.store(false, Ordering::SeqCst);
        self.start_listener().await;
        self.setup().await
    }

    /// Prepare the same channel again (after `chrome.debugger` re-attached).
    pub async fn reprepare(&self) -> Result<()> {
        *self.world.lock().await = None;
        *self.last.lock().await = None;
        self.setup().await
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(h) = self.listener.lock().await.take() {
            h.abort();
        }
    }

    async fn start_listener(&self) {
        let channel = self.channel.read().await.clone();
        let mut rx = channel.subscribe();
        let world = self.world.clone();
        let dialog = self.dialog.clone();
        let last_url = self.last_url.clone();
        let hold = self.hold.clone();
        let closed = self.closed.clone();
        let handle = tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                if !channel.owns(&ev) {
                    continue;
                }
                match ev.method.as_str() {
                    "Runtime.executionContextDestroyed" => {
                        let destroyed = ev.params.get("executionContextId").and_then(Value::as_i64);
                        let mut w = world.lock().await;
                        if destroyed.is_some() && *w == destroyed {
                            *w = None;
                        }
                    }
                    "Runtime.executionContextsCleared" => *world.lock().await = None,
                    "Page.frameNavigated" => {
                        let frame = ev.params.get("frame").cloned().unwrap_or(Value::Null);
                        if frame.get("parentId").is_none() {
                            *world.lock().await = None;
                            if let Some(u) = frame.get("url").and_then(Value::as_str) {
                                *last_url.lock().await = u.to_string();
                            }
                        }
                    }
                    "Page.javascriptDialogOpening" => {
                        *dialog.lock().await = Some(json!({
                            "type": ev.params.get("type").cloned().unwrap_or(Value::Null),
                            "message": ev.params.get("message").cloned().unwrap_or(Value::Null),
                        }));
                    }
                    "Page.javascriptDialogClosed" => *dialog.lock().await = None,
                    "Sen.detached" => {
                        *hold.lock().await = Hold::Detached;
                        *world.lock().await = None;
                    }
                    "Sen.userActive" => {
                        let mut h = hold.lock().await;
                        if *h == Hold::None {
                            *h = Hold::UserActive;
                        }
                    }
                    "Sen.tabClosed" | "Sen.connectionClosed" => {
                        closed.store(true, Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
        });
        *self.listener.lock().await = Some(handle);
    }

    async fn setup(&self) -> Result<()> {
        self.send("Page.enable", json!({})).await?;
        self.send("Runtime.enable", json!({})).await?;
        // Keeps animation frames and timers running in an owned background tab
        // without activating it in front of the person.
        self.send("Emulation.setFocusEmulationEnabled", json!({ "enabled": true })).await?;
        if self.emulate_viewport {
            self.send(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": 1120, "height": 780, "deviceScaleFactor": 1, "mobile": false }),
            )
            .await?;
        }
        Ok(())
    }

    async fn send_raw(&self, method: &str, params: Value) -> std::result::Result<Value, CdpError> {
        allowlist::check(method, &params).map_err(CdpError::Blocked)?;
        let channel = self.channel.read().await.clone();
        channel.send(method, params).await
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_raw(method, params).await.map_err(BrowserError::from)
    }

    async fn ensure_world(&self) -> std::result::Result<i64, CdpError> {
        let mut w = self.world.lock().await;
        if let Some(id) = *w {
            return Ok(id);
        }
        let tree = self.send_raw("Page.getFrameTree", json!({})).await?;
        let frame_id = tree
            .pointer("/frameTree/frame/id")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Other("the tab has no main frame yet".into()))?
            .to_string();
        let created = self
            .send_raw(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame_id, "worldName": "senclaw", "grantUniveralAccess": false }),
            )
            .await?;
        let id = created
            .get("executionContextId")
            .and_then(Value::as_i64)
            .ok_or_else(|| CdpError::Other("Page.createIsolatedWorld returned no context".into()))?;
        *w = Some(id);
        Ok(id)
    }

    /// Call one approved script in the isolated world. A context that vanished
    /// under the call (navigation) is recreated once.
    async fn call(&self, script: &Script, args: Vec<Value>, await_promise: bool) -> std::result::Result<Value, CallError> {
        for _ in 0..2 {
            let context = self.ensure_world().await.map_err(CallError::Cdp)?;
            let params = json!({
                "functionDeclaration": script.declaration(),
                "executionContextId": context,
                "arguments": args.iter().map(|a| json!({ "value": a })).collect::<Vec<_>>(),
                "returnByValue": true,
                "awaitPromise": await_promise,
            });
            match self.send_raw("Runtime.callFunctionOn", params).await {
                Ok(res) => {
                    if let Some(ex) = res.get("exceptionDetails") {
                        let text = ex
                            .pointer("/exception/description")
                            .and_then(Value::as_str)
                            .or_else(|| ex.get("text").and_then(Value::as_str))
                            .unwrap_or("the page threw")
                            .to_string();
                        return Err(CallError::Page(text));
                    }
                    return Ok(res.pointer("/result/value").cloned().unwrap_or(Value::Null));
                }
                Err(e) if e.is_context_lost() => {
                    *self.world.lock().await = None;
                    continue;
                }
                Err(e) => return Err(CallError::Cdp(e)),
            }
        }
        Err(CallError::Page("the document changed during the call".into()))
    }

    async fn check_available(&self) -> Result<()> {
        if self.is_closed() {
            return Err(BrowserError::NotFound(format!("tab {} is closed", self.id)));
        }
        match self.hold().await {
            Hold::None => Ok(()),
            Hold::Handover => Err(BrowserError::HandoverActive),
            Hold::UserActive => Err(BrowserError::UserActive),
            Hold::Detached => Err(BrowserError::Detached),
        }
    }

    /// Observe the page in one call.
    pub async fn observe(&self, text_chars: Option<u64>, max_candidates: Option<u64>) -> Result<Value> {
        let _op = self.op.lock().await;
        self.check_available().await?;
        self.observe_locked(
            text_chars.unwrap_or(DEFAULT_TEXT_CHARS).clamp(200, 20_000),
            max_candidates.unwrap_or(DEFAULT_MAX_CANDIDATES).clamp(1, 250),
        )
        .await
    }

    async fn observe_locked(&self, text_chars: u64, max_candidates: u64) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Some(dialog) = self.dialog.lock().await.clone() {
                let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
                let url = self.last_url().await;
                let (stored, public) = observation::dialog_observation(id, &self.id, &url, &dialog);
                *self.last.lock().await = Some(stored);
                return Ok(public);
            }
            match self
                .call(&scripts::SNAPSHOT, vec![json!(max_candidates), json!(text_chars), json!(false)], false)
                .await
            {
                Ok(raw) if !raw.is_null() => {
                    let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
                    let (stored, public) =
                        observation::split(id, &self.id, raw, text_chars, max_candidates, started.elapsed().as_millis());
                    *self.last_url.lock().await = stored.url.clone();
                    *self.last.lock().await = Some(stored);
                    return Ok(public);
                }
                Err(CallError::Cdp(CdpError::Closed)) => {
                    return Err(BrowserError::Internal("the browser connection closed".into()))
                }
                Err(CallError::Cdp(CdpError::Blocked(m))) => return Err(BrowserError::Blocked(m)),
                // No body yet, or the document is navigating: try again shortly.
                _ => {}
            }
            if started.elapsed() > OBSERVE_DEADLINE {
                return Err(BrowserError::Internal("the page did not settle within 10 s".into()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Execute one observed action. The page must still be the observed one:
    /// click/select compare the target's scoped guard, everything else the full
    /// semantic marker. Nothing is sent to the page when the check fails.
    pub async fn act(&self, observation_id: u64, action_id: &str, text: Option<&str>) -> Result<Value> {
        let _op = self.op.lock().await;
        self.check_available().await?;
        let stored = self.last.lock().await.clone().ok_or(BrowserError::StalePage)?;
        if stored.id != observation_id {
            return Err(BrowserError::StalePage);
        }
        let action = stored
            .action(action_id)
            .cloned()
            .ok_or_else(|| BrowserError::InvalidTarget(format!("observation {observation_id} has no action {action_id}")))?;
        let kind = action.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
        let node = action.get("node").and_then(Value::as_i64).unwrap_or(0);

        if self.dialog.lock().await.is_some() && kind != "dialog" {
            return Err(BrowserError::DialogOpen);
        }
        if kind == "fill" {
            if action.get("sensitive").and_then(Value::as_bool) == Some(true) {
                return Err(BrowserError::Blocked(
                    "credential-like field: hand the tab to the person instead of typing".into(),
                ));
            }
            if text.is_none() {
                return Err(BrowserError::BadRequest("a fill action needs text".into()));
            }
        }

        match kind.as_str() {
            "click" | "select" => {
                let current = self.call(&scripts::GUARD, vec![json!(node)], false).await.map_err(|_| BrowserError::StalePage)?;
                let expected = json!([stored.page_key, stored.guards.get(&node.to_string()).cloned().unwrap_or(Value::Null)]);
                if current != expected {
                    return Err(BrowserError::StalePage);
                }
            }
            "dialog" => {}
            _ => {
                let current = self
                    .call(
                        &scripts::SNAPSHOT,
                        vec![json!(stored.max_candidates), json!(stored.text_chars), json!(true)],
                        false,
                    )
                    .await
                    .map_err(|_| BrowserError::StalePage)?;
                if current.get("marker") != Some(&stored.marker) {
                    return Err(BrowserError::StalePage);
                }
            }
        }

        // Consume the observation before any input: a retry of this request can
        // never repeat the action.
        *self.last.lock().await = None;
        let started = Instant::now();
        match kind.as_str() {
            "scroll" => {
                let delta = action.get("delta").and_then(Value::as_f64).unwrap_or(560.0);
                self.send(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseWheel", "x": stored.w / 2.0, "y": stored.h * 0.8, "deltaX": 0, "deltaY": delta }),
                )
                .await?;
            }
            "wait" => tokio::time::sleep(Duration::from_millis(100)).await,
            "key" => self.press_key(action.get("key").and_then(Value::as_str).unwrap_or("Enter")).await?,
            "back" => self.go_back().await?,
            "dialog" => {
                let accept = action.get("accept").and_then(Value::as_bool).unwrap_or(false);
                self.send("Page.handleJavaScriptDialog", json!({ "accept": accept })).await?;
                *self.dialog.lock().await = None;
            }
            "click" | "fill" | "select" => {
                let request = json!({ "node": node, "kind": kind, "value": action.get("value").cloned().unwrap_or(Value::Null) });
                let resolved = match self.call(&scripts::ACT, vec![request], false).await {
                    Ok(v) => v,
                    // A `<select>` may already have fired its change event.
                    Err(e) if kind == "select" => return Err(BrowserError::UncertainMutation(e.to_string())),
                    Err(_) => return Err(BrowserError::StalePage),
                };
                if resolved.get("ok") != Some(&Value::Bool(true)) {
                    let reason = resolved.get("reason").and_then(Value::as_str).unwrap_or("unknown");
                    return Err(if reason == "covered" {
                        BrowserError::TargetCovered(format!(" ({})", resolved.get("by").and_then(Value::as_str).unwrap_or("?")))
                    } else {
                        BrowserError::InvalidTarget(reason.to_string())
                    });
                }
                if kind != "select" {
                    let x = resolved.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                    let y = resolved.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                    self.click_at(x, y).await?;
                    if kind == "fill" {
                        self.replace_text(text.unwrap_or_default()).await?;
                    }
                }
            }
            other => return Err(BrowserError::InvalidTarget(format!("unsupported action kind {other}"))),
        }
        let executed_ms = started.elapsed().as_millis() as u64;

        let settle_started = Instant::now();
        let dialog_opened = self.dialog.lock().await.is_some();
        match kind.as_str() {
            _ if dialog_opened => {}
            "wait" | "dialog" => {}
            "back" => self.wait_for_load(Duration::from_secs(5)).await,
            _ => {
                let _ = tokio::time::timeout(SETTLE_DEADLINE, self.call(&scripts::SETTLE, vec![json!(node), json!(kind)], true)).await;
            }
        }
        let settle_ms = settle_started.elapsed().as_millis() as u64;

        // The action happened; an observation failing now (a slow navigation)
        // must not erase that. The caller observes again.
        let (observation, observation_error) = match self.observe_locked(stored.text_chars, stored.max_candidates).await {
            Ok(o) => (o, Value::Null),
            Err(e) => (Value::Null, json!({ "error": e.to_string(), "code": e.code() })),
        };
        Ok(json!({
            "executed": action_id,
            "kind": kind,
            "executed_ms": executed_ms,
            "settle_ms": settle_ms,
            "observation": observation,
            "observation_error": observation_error,
        }))
    }

    async fn click_at(&self, x: f64, y: f64) -> Result<()> {
        self.send("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y })).await?;
        self.send(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 }),
        )
        .await?;
        self.send_input(json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 }), "Input.dispatchMouseEvent")
            .await
    }

    /// Send the input event that may run a page handler. A handler that opens
    /// `alert`/`confirm`/`prompt` blocks the renderer — and with it the
    /// acknowledgement of this very event — until the dialog is answered. The
    /// input was delivered, so a dialog appearing counts as done.
    async fn send_input(&self, params: Value, method: &str) -> Result<()> {
        let sent = self.send(method, params);
        tokio::pin!(sent);
        loop {
            tokio::select! {
                r = &mut sent => return r.map(|_| ()),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {
                    if self.dialog.lock().await.is_some() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Replace a field's contents: the browser's own select-all, then trusted
    /// text insertion (or Backspace to clear).
    async fn replace_text(&self, text: &str) -> Result<()> {
        let modifiers = if cfg!(target_os = "macos") { 4 } else { 2 };
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": "a", "code": "KeyA", "modifiers": modifiers, "commands": ["selectAll"] }),
        )
        .await?;
        self.send("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": "a", "code": "KeyA", "modifiers": modifiers }))
            .await?;
        if text.is_empty() {
            self.press_key("Backspace").await
        } else {
            self.send("Input.insertText", json!({ "text": text })).await.map(|_| ())
        }
    }

    async fn press_key(&self, key: &str) -> Result<()> {
        let (code, vk, text) = match key {
            "Enter" => ("Enter", 13, "\r"),
            "Escape" => ("Escape", 27, ""),
            "Tab" => ("Tab", 9, ""),
            "Backspace" => ("Backspace", 8, ""),
            "ArrowDown" => ("ArrowDown", 40, ""),
            "ArrowUp" => ("ArrowUp", 38, ""),
            other => return Err(BrowserError::InvalidTarget(format!("unsupported key {other}"))),
        };
        let mut down = json!({ "type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
        if !text.is_empty() {
            down["text"] = json!(text);
            down["unmodifiedText"] = json!(text);
        }
        self.send_input(down, "Input.dispatchKeyEvent").await?;
        if self.dialog.lock().await.is_some() {
            return Ok(());
        }
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
        )
        .await?;
        Ok(())
    }

    async fn go_back(&self) -> Result<()> {
        let history = self.send("Page.getNavigationHistory", json!({})).await?;
        let index = history.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
        if index <= 0 {
            return Err(BrowserError::InvalidTarget("there is no previous page".into()));
        }
        let entry = history
            .pointer(&format!("/entries/{}/id", index - 1))
            .cloned()
            .ok_or_else(|| BrowserError::Internal("navigation history without the previous entry".into()))?;
        self.send("Page.navigateToHistoryEntry", json!({ "entryId": entry })).await?;
        Ok(())
    }

    /// Wait for the main frame to finish loading (or give up after `limit`).
    async fn wait_for_load(&self, limit: Duration) {
        let channel = self.channel.read().await.clone();
        let mut rx = channel.subscribe();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(ev) if channel.owns(&ev) && matches!(ev.method.as_str(), "Page.loadEventFired" | "Page.navigatedWithinDocument") => return,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }

    /// Open `url` in this tab and observe the result.
    pub async fn navigate(&self, url: &str) -> Result<Value> {
        let _op = self.op.lock().await;
        self.check_available().await?;
        check_url(url)?;
        *self.last.lock().await = None;
        let channel = self.channel.read().await.clone();
        let mut rx = channel.subscribe();
        let res = self.send("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = res.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            return Err(BrowserError::BadRequest(format!("navigation to {url} failed: {err}")));
        }
        let frame = res.get("frameId").and_then(Value::as_str).unwrap_or_default().to_string();
        let same_document = res.get("loaderId").is_none();
        if !same_document {
            let deadline = tokio::time::Instant::now() + LOAD_DEADLINE;
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(ev) if channel.owns(&ev) => {
                            let done = ev.method == "Page.loadEventFired"
                                || (ev.method == "Page.frameStoppedLoading"
                                    && ev.params.get("frameId").and_then(Value::as_str) == Some(frame.as_str()));
                            if done { break; }
                        }
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    },
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            }
        }
        self.observe_locked(DEFAULT_TEXT_CHARS, DEFAULT_MAX_CANDIDATES).await
    }

    /// The whole page's readable text, for an LLM to read.
    pub async fn read(&self, max_chars: Option<u64>) -> Result<Value> {
        let _op = self.op.lock().await;
        self.check_available().await?;
        if self.dialog.lock().await.is_some() {
            return Err(BrowserError::DialogOpen);
        }
        self.call(&scripts::READ, vec![json!(max_chars.unwrap_or(20_000).clamp(500, 200_000))], false)
            .await
            .map_err(|e| BrowserError::Internal(e.to_string()))
    }

    /// A JPEG of the viewport, base64-encoded.
    pub async fn screenshot(&self) -> Result<Value> {
        let _op = self.op.lock().await;
        self.check_available().await?;
        let shot = self.send("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 72 })).await?;
        Ok(json!({ "mime": "image/jpeg", "data": shot.get("data").cloned().unwrap_or(Value::Null) }))
    }

    pub async fn describe(&self) -> Value {
        json!({
            "id": self.id,
            "owner": self.owner,
            "driver": self.driver,
            "url": self.last_url().await,
            "hold": self.hold().await,
            "closed": self.is_closed(),
        })
    }
}

/// Only web pages. `javascript:`, `file:`, `chrome:` and friends are refused.
pub fn check_url(url: &str) -> Result<()> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower == "about:blank" {
        Ok(())
    } else {
        Err(BrowserError::Blocked(format!("only http(s) pages can be opened, not {url}")))
    }
}

#[cfg(test)]
mod tests {
    use super::check_url;

    #[test]
    fn only_web_urls_open() {
        assert!(check_url("https://example.com/a").is_ok());
        assert!(check_url("http://127.0.0.1:8080/").is_ok());
        assert!(check_url("about:blank").is_ok());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("chrome://settings").is_err());
    }
}
