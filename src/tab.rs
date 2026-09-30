//! One tab the agent owns: observe it in one call, act on it with trusted
//! input only after proving the page is still the one observed, and keep the
//! bookkeeping (isolated world, dialogs, handover) straight.
//!
//! The same code runs for both drivers; only the [`TabChannel`] differs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::allowlist;
use crate::cdp::{CdpError, CdpEvent, TabChannel};
use crate::error::{BrowserError, Result};
use crate::observation::{self, Stored, DEFAULT_MAX_CANDIDATES, DEFAULT_TEXT_CHARS};
use crate::scripts::{self, Script};

/// How long an observation may wait for a navigating page to produce a body.
const OBSERVE_DEADLINE: Duration = Duration::from_secs(10);
/// Settle waits are capped at 200 ms inside the page; this bounds a wedged one.
const SETTLE_DEADLINE: Duration = Duration::from_millis(1500);
/// The settle's share once the action asked for a navigation.
const NAVIGATING_SETTLE: Duration = Duration::from_millis(120);
/// Navigation: how long a new document may take to arrive and be parsed.
const LOAD_DEADLINE: Duration = Duration::from_secs(15);
/// A parsed document gets this long to finish rendering before it is
/// observed as it is.
const READY_GRACE: Duration = Duration::from_secs(4);
/// With no word from the network, what the page shows has to stay the same
/// this long to count as rendered (a page that streams video never goes quiet).
const STABLE_FOR: Duration = Duration::from_millis(1500);
const READY_POLL: Duration = Duration::from_millis(200);
/// How long a navigation that stopped without a document may still produce one.
const CANCEL_LINGER: Duration = Duration::from_millis(400);
/// How long a navigation that was asked for may take to get under way.
const START_LINGER: Duration = Duration::from_secs(1);
/// A page showing this much is taken as it is once its load event fired:
/// server-rendered pages are complete then. A sparser one that loads scripts
/// may be an app shell still fetching what it will show.
const RICH_TEXT_CHARS: usize = 400;
const RICH_CONTROLS: usize = 40;

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

/// Where the main frame is in a navigation, read off the page's own events.
#[derive(Debug, Default, Clone)]
struct NavWatch {
    /// A new document was asked for.
    started: bool,
    /// It is on its way: until it commits, the page's other events are about
    /// the document being left.
    coming: bool,
    /// The document that committed, by its loader id.
    loader: Option<String>,
    /// Its HTML is parsed (`DOMContentLoaded`).
    parsed: bool,
    /// Its load event fired.
    loaded: bool,
    /// Its network has at most two requests in flight (Chrome's own idle
    /// detection, half a second of it): only stragglers are left.
    almost_idle: bool,
    /// Its network has nothing in flight.
    idle: bool,
    /// The navigation ended with nothing further to wait for.
    settled: Option<&'static str>,
}

impl NavWatch {
    /// A navigation to another document is under way.
    fn coming() -> NavWatch {
        NavWatch { started: true, coming: true, ..NavWatch::default() }
    }

    /// Whether the network has nothing more to bring that matters. Before the
    /// load event what is in flight is the page's own resources: two left are
    /// stragglers (an ad, a tracker, a video). After it, what is in flight is
    /// what the page's scripts asked for — one request may be all its data.
    fn quiet(&self) -> bool {
        if self.loaded {
            self.idle
        } else {
            self.almost_idle
        }
    }

    fn see(&mut self, ev: &CdpEvent, main_frame: &str) {
        let param = |key: &str| ev.params.get(key).and_then(Value::as_str);
        let main = || param("frameId") == Some(main_frame);
        match ev.method.as_str() {
            "Page.frameNavigated" if ev.params.pointer("/frame/parentId").is_none() => {
                let loader = ev.params.pointer("/frame/loaderId").and_then(Value::as_str).map(str::to_string);
                *self = NavWatch { started: true, loader, ..NavWatch::default() };
                // A page restored from the back/forward cache is already complete.
                if param("type") == Some("BackForwardCacheRestore") {
                    self.settled = Some("restored");
                }
            }
            // `frameStartedLoading` is no use here: the history API fires it
            // too, around a navigation that never leaves the document.
            "Page.frameStartedNavigating" if main() => {
                if matches!(param("navigationType"), Some("sameDocument" | "historySameDocument")) {
                    return;
                }
                *self = NavWatch::coming();
            }
            // A link that opens a new tab, or a download, leaves this page where it is.
            "Page.frameRequestedNavigation" if main() && matches!(param("disposition"), None | Some("currentTab")) => {
                self.started = true
            }
            // Loading ended. For the committed document that is its load; with
            // a document still to come, the navigation was cancelled (a
            // download, a 204).
            "Page.frameStoppedLoading" if main() => match (&self.loader, self.coming) {
                (Some(_), false) => self.loaded = true,
                _ => self.settled = self.settled.or(Some("cancelled")),
            },
            _ if self.coming => return,
            "Page.domContentEventFired" if self.loader.is_some() => self.parsed = true,
            "Page.loadEventFired" if self.loader.is_some() => {
                self.parsed = true;
                self.loaded = true;
            }
            // A fragment or the history API: the document stays.
            "Page.navigatedWithinDocument" if main() && self.loader.is_none() => self.settled = Some("same_document"),
            "Page.lifecycleEvent" if main() && self.loader.is_some() && param("loaderId") == self.loader.as_deref() => {
                match param("name") {
                    Some("networkAlmostIdle") => self.almost_idle = true,
                    Some("networkIdle") => (self.almost_idle, self.idle) = (true, true),
                    _ => return,
                }
            }
            _ => return,
        }
        tracing::debug!("page event {} -> {self:?}", ev.method);
    }
}

/// What a page shows at one moment of a navigation, cheap enough to ask for
/// several times a second.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shown {
    /// A digest of the snapshot marker: changes when what the page shows does.
    digest: u64,
    text: usize,
    controls: usize,
    /// Scripts the document loads: without any it cannot fill in later.
    scripts: u64,
}

impl Shown {
    fn parse(probe: &Value) -> Option<Shown> {
        let number = |key: &str| probe.get(key).and_then(Value::as_f64);
        Some(Shown {
            digest: number("digest")? as u64,
            text: number("text")? as usize,
            controls: number("controls")? as usize,
            scripts: number("scripts").unwrap_or(1.0) as u64,
        })
    }

    fn blank(&self) -> bool {
        self.text + self.controls == 0
    }

    /// Whether more may still arrive once the load event fired: an app shell
    /// is a sparse page that loads scripts.
    fn may_fill_in(&self) -> bool {
        self.scripts > 0 && self.text < RICH_TEXT_CHARS && self.controls < RICH_CONTROLS
    }
}

pub struct Tab {
    pub id: String,
    pub owner: Option<String>,
    pub driver: DriverKind,
    /// Managed: the CDP target id. Extension: the Chrome tab id.
    target: RwLock<String>,
    channel: RwLock<Arc<dyn TabChannel>>,
    /// The main frame's id, as the page's navigation events name it.
    frame: Arc<Mutex<String>>,
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
            frame: Arc::new(Mutex::new(String::new())),
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
        let main_frame = self.frame.clone();
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
                            if let Some(id) = frame.get("id").and_then(Value::as_str) {
                                *main_frame.lock().await = id.to_string();
                            }
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
        // Chrome's own word on when a document's network goes quiet. An
        // extension built before this was asked for refuses it; navigation
        // then falls back on what the page shows.
        if let Err(e) = self.send("Page.setLifecycleEventsEnabled", json!({ "enabled": true })).await {
            tracing::debug!("no lifecycle events for tab {}: {e}", self.id);
        }
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
        *self.frame.lock().await = frame_id.clone();
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
        // Hear what the action does to the page: a click may start a navigation.
        let channel = self.channel.read().await.clone();
        let mut events = channel.subscribe();
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

        // An action that started a navigation is followed to the document it
        // brings. Chrome holds every call to the page until that document
        // commits, so the settle gets only a moment then: enough for a
        // navigation that stays in the document, no wait on a page being left.
        let frame = self.frame.lock().await.clone();
        let mut watch = NavWatch { started: kind == "back", ..NavWatch::default() };
        let hear = |watch: &mut NavWatch, events: &mut tokio::sync::broadcast::Receiver<CdpEvent>| loop {
            match events.try_recv() {
                Ok(ev) if channel.owns(&ev) => watch.see(&ev, &frame),
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        };
        let settle_started = Instant::now();
        let dialog_opened = self.dialog.lock().await.is_some();
        hear(&mut watch, &mut events);
        if !dialog_opened && !matches!(kind.as_str(), "wait" | "dialog" | "back") {
            let limit = if watch.started { NAVIGATING_SETTLE } else { SETTLE_DEADLINE };
            let _ = tokio::time::timeout(limit, self.call(&scripts::SETTLE, vec![json!(node), json!(kind)], true)).await;
            hear(&mut watch, &mut events);
        }
        let settle_ms = settle_started.elapsed().as_millis() as u64;

        let ready_started = Instant::now();
        let ready = if watch.started && !dialog_opened && kind != "dialog" {
            Some(self.wait_ready(&mut events, &channel, &mut watch, &frame).await)
        } else {
            None
        };
        let ready_ms = ready_started.elapsed().as_millis() as u64;

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
            "ready_ms": ready_ms,
            "ready_by": ready,
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
        let previous = history
            .pointer(&format!("/entries/{}", index - 1))
            .cloned()
            .ok_or_else(|| BrowserError::Internal("navigation history without the previous entry".into()))?;
        // History can hold what `Page.navigate` refuses (a shared tab's `file:` page).
        if !previous.get("url").and_then(Value::as_str).is_some_and(crate::allowlist::is_web_url) {
            return Err(BrowserError::InvalidTarget("the previous page is not a web page".into()));
        }
        let entry = previous.get("id").cloned().unwrap_or(Value::Null);
        self.send("Page.navigateToHistoryEntry", json!({ "entryId": entry })).await?;
        Ok(())
    }

    /// Wait until the document a navigation brings is worth observing, and say
    /// what ended the wait.
    ///
    /// The load event alone is the wrong signal in both directions. A page may
    /// hold it back for as long as a video stream, an ad or a tracker takes —
    /// while everything a person would act on is already there. And an app
    /// shell fires it before fetching what it will show. So a parsed document
    /// is ready when its load event fired and it is not such a shell (it
    /// shows a full page, or loads no script that could fill it in); or when
    /// its network went quiet and what it shows stopped changing; or, with a
    /// network that never goes quiet, when what it shows has stayed the same
    /// for a while.
    async fn wait_ready(
        &self,
        rx: &mut tokio::sync::broadcast::Receiver<CdpEvent>,
        channel: &Arc<dyn TabChannel>,
        watch: &mut NavWatch,
        frame: &str,
    ) -> &'static str {
        let deadline = tokio::time::Instant::now() + LOAD_DEADLINE;
        let mut grace: Option<tokio::time::Instant> = None;
        let mut last: Option<Shown> = None;
        let mut unchanged_since = tokio::time::Instant::now();
        let mut next_poll = tokio::time::Instant::now();
        let mut cancelled: Option<tokio::time::Instant> = None;
        let mut unanswered: Option<tokio::time::Instant> = None;
        loop {
            // Everything already heard counts before anything is concluded.
            let before = (watch.loaded, watch.quiet());
            loop {
                match rx.try_recv() {
                    Ok(ev) if channel.owns(&ev) => watch.see(&ev, frame),
                    Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            let now = tokio::time::Instant::now();
            let mut linger = None;
            match watch.settled {
                // Loading stopped with no new document: cancelled — or a page
                // coming back from the back/forward cache, whose commit is
                // reported a moment after.
                Some("cancelled") => {
                    let until = *cancelled.get_or_insert(now + CANCEL_LINGER);
                    if now >= until {
                        return "cancelled";
                    }
                    linger = Some(until);
                }
                Some(why) => return why,
                None => cancelled = None,
            }
            // Asked for, and nothing under way: a navigation that was dropped
            // (the page refused it, or there was nowhere to go back to).
            if !watch.coming && watch.loader.is_none() {
                let until = *unanswered.get_or_insert(now + START_LINGER);
                if now >= until {
                    return "cancelled";
                }
                linger = Some(until);
            }
            // A dialog blocks the page: nothing more will load until it is answered.
            if self.dialog.lock().await.is_some() {
                return "dialog";
            }
            if now >= deadline {
                return "deadline";
            }
            if !watch.parsed {
                // A redirect started a new document: what was seen no longer counts.
                (grace, last) = (None, None);
            } else if now >= *grace.get_or_insert(now + READY_GRACE) {
                return "grace";
            } else if now >= next_poll || before != (watch.loaded, watch.quiet()) {
                next_poll = now + READY_POLL;
                let args = vec![json!(DEFAULT_MAX_CANDIDATES), json!(DEFAULT_TEXT_CHARS), json!("probe")];
                let shown = self.call(&scripts::SNAPSHOT, args, false).await.ok().as_ref().and_then(Shown::parse);
                if watch.loaded && shown.is_some_and(|s| !s.may_fill_in()) {
                    return "load";
                }
                if shown.is_some() && shown == last && shown.is_some_and(|s| !s.blank()) {
                    if watch.quiet() {
                        return "quiet";
                    }
                    if unchanged_since.elapsed() >= STABLE_FOR {
                        return "stable";
                    }
                } else {
                    last = shown;
                    unchanged_since = tokio::time::Instant::now();
                }
                continue;
            }
            let wake = linger.unwrap_or(if watch.parsed { next_poll.min(deadline) } else { deadline });
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(ev) if channel.owns(&ev) => watch.see(&ev, frame),
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return "closed",
                },
                _ = tokio::time::sleep_until(wake) => {}
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
        let waiting = Instant::now();
        let ready = if same_document {
            "same_document"
        } else {
            let mut watch = NavWatch::coming();
            self.wait_ready(&mut rx, &channel, &mut watch, &frame).await
        };
        let ready_ms = waiting.elapsed().as_millis() as u64;
        let mut observation = self.observe_locked(DEFAULT_TEXT_CHARS, DEFAULT_MAX_CANDIDATES).await?;
        observation["timing_ms"]["ready"] = json!(ready_ms);
        observation["timing_ms"]["ready_by"] = json!(ready);
        Ok(observation)
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
    use super::*;

    const MAIN: &str = "F1";

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent { method: method.to_string(), params, session: None, tab: None }
    }

    fn watch(start: NavWatch, events: &[(&str, Value)]) -> NavWatch {
        let mut w = start;
        for (method, params) in events {
            w.see(&event(method, params.clone()), MAIN);
        }
        w
    }

    fn commit(loader: &str) -> (&'static str, Value) {
        ("Page.frameNavigated", json!({ "frame": { "id": MAIN, "loaderId": loader }, "type": "Navigation" }))
    }

    fn idle(loader: &str) -> (&'static str, Value) {
        ("Page.lifecycleEvent", json!({ "frameId": MAIN, "loaderId": loader, "name": "networkIdle" }))
    }

    /// The events Chrome reports for a click on a link, in its order.
    #[test]
    fn a_clicked_link_is_followed_to_the_document_it_brings() {
        let asked = [
            ("Page.frameRequestedNavigation", json!({ "frameId": MAIN, "reason": "anchorClick" })),
            ("Page.frameStartedNavigating", json!({ "frameId": MAIN, "navigationType": "differentDocument", "loaderId": "L2" })),
            ("Page.frameStartedLoading", json!({ "frameId": MAIN })),
        ];
        let w = watch(NavWatch::default(), &asked);
        assert!(w.started && w.coming && !w.parsed && w.settled.is_none(), "{w:?}");

        let w = watch(w, &[commit("L2"), ("Page.domContentEventFired", json!({}))]);
        assert!(w.parsed && !w.loaded && !w.quiet() && !w.coming, "{w:?}");
        // Two requests left while the page loads are stragglers; two left after
        // its load event may be all the data an app is waiting for.
        let almost = ("Page.lifecycleEvent", json!({ "frameId": MAIN, "loaderId": "L2", "name": "networkAlmostIdle" }));
        let w2 = watch(w.clone(), &[almost]);
        assert!(w2.quiet(), "{w2:?}");
        assert!(!watch(w2, &[("Page.loadEventFired", json!({}))]).quiet());
        let w = watch(w, &[idle("L2"), ("Page.loadEventFired", json!({})), ("Page.frameStoppedLoading", json!({ "frameId": MAIN }))]);
        assert!(w.quiet() && w.loaded && w.settled.is_none(), "{w:?}");
    }

    /// Until the new document commits, the page being left goes on reporting:
    /// its load event, its idle network. None of that is the new page's.
    #[test]
    fn the_page_being_left_does_not_answer_for_the_new_one() {
        let w = watch(
            NavWatch::coming(),
            &[("Page.domContentEventFired", json!({})), ("Page.loadEventFired", json!({})), idle("L1")],
        );
        assert!(!w.parsed && !w.loaded && !w.quiet(), "{w:?}");
        // A document in a subframe is not the page either.
        let w = watch(
            w,
            &[
                ("Page.frameNavigated", json!({ "frame": { "id": "F2", "parentId": MAIN, "loaderId": "X" } })),
                ("Page.frameStoppedLoading", json!({ "frameId": "F2" })),
                ("Page.frameStartedNavigating", json!({ "frameId": "F2", "navigationType": "differentDocument" })),
            ],
        );
        assert!(w.coming && w.loader.is_none() && w.settled.is_none(), "{w:?}");
        // Nor is the idle network of an earlier document.
        let w = watch(w, &[commit("L2"), ("Page.domContentEventFired", json!({})), idle("L1")]);
        assert!(w.parsed && !w.quiet(), "{w:?}");
    }

    /// The history API fires loading events around a navigation that never
    /// leaves the document (seen on real pages right after their load event).
    #[test]
    fn the_history_api_is_not_a_new_document() {
        let loaded = watch(NavWatch::coming(), &[commit("L1"), ("Page.loadEventFired", json!({})), idle("L1")]);
        let blip = [
            ("Page.frameStartedLoading", json!({ "frameId": MAIN })),
            ("Page.navigatedWithinDocument", json!({ "frameId": MAIN, "navigationType": "historyApi" })),
            ("Page.frameStoppedLoading", json!({ "frameId": MAIN })),
        ];
        let w = watch(loaded, &blip);
        assert!(w.loaded && w.quiet() && w.settled.is_none() && w.loader.as_deref() == Some("L1"), "{w:?}");

        // On a page that was not navigating, it asks for nothing to wait on…
        let w = watch(NavWatch::default(), &blip);
        assert!(!w.started, "{w:?}");
        // …and going back within the document ends at once.
        let mut back = vec![("Page.frameStartedNavigating", json!({ "frameId": MAIN, "navigationType": "historySameDocument" }))];
        back.extend(blip);
        let w = watch(NavWatch { started: true, ..NavWatch::default() }, &back);
        assert_eq!(w.settled, Some("same_document"), "{w:?}");
    }

    #[test]
    fn a_restored_or_cancelled_navigation_has_nothing_to_wait_for() {
        // Back/forward cache: loading stops, then the restored page is reported.
        let back = [
            ("Page.frameStartedNavigating", json!({ "frameId": MAIN, "navigationType": "historyDifferentDocument" })),
            ("Page.frameStoppedLoading", json!({ "frameId": MAIN })),
        ];
        let w = watch(NavWatch { started: true, ..NavWatch::default() }, &back);
        assert_eq!(w.settled, Some("cancelled"), "until the restored page is reported: {w:?}");
        let restored = ("Page.frameNavigated", json!({ "frame": { "id": MAIN, "loaderId": "L0" }, "type": "BackForwardCacheRestore" }));
        assert_eq!(watch(w, &[restored]).settled, Some("restored"));
        // A redirect after the page was parsed starts over.
        let w = watch(NavWatch::coming(), &[commit("L1"), ("Page.domContentEventFired", json!({}))]);
        let w = watch(w, &[("Page.frameStartedNavigating", json!({ "frameId": MAIN, "navigationType": "differentDocument" }))]);
        assert!(w.coming && !w.parsed && w.loader.is_none(), "{w:?}");
    }

    #[test]
    fn an_app_shell_is_a_sparse_page_that_loads_scripts() {
        let shown = |text: usize, controls: usize, scripts: u64| {
            Shown::parse(&json!({ "digest": 7, "text": text, "controls": controls, "scripts": scripts })).unwrap()
        };
        assert!(shown(95, 15, 12).may_fill_in(), "a navigation bar and a spinner");
        assert!(!shown(95, 15, 0).may_fill_in(), "no script: what it shows is what there is");
        assert!(!shown(1500, 5, 12).may_fill_in(), "a page of text");
        assert!(!shown(40, 60, 12).may_fill_in(), "a page of controls");
        assert!(shown(0, 0, 3).blank() && !shown(0, 1, 3).blank());
        // A probe that is not one (the page was navigating) is no answer.
        assert!(Shown::parse(&json!({ "marker": [] })).is_none());
        assert!(Shown::parse(&Value::Null).is_none());
    }

    #[test]
    fn a_link_that_opens_elsewhere_is_not_a_navigation_of_this_page() {
        for disposition in ["newTab", "newWindow", "download"] {
            let ev = ("Page.frameRequestedNavigation", json!({ "frameId": MAIN, "reason": "anchorClick", "disposition": disposition }));
            assert!(!watch(NavWatch::default(), &[ev]).started, "{disposition}");
        }
        let here = ("Page.frameRequestedNavigation", json!({ "frameId": MAIN, "reason": "anchorClick", "disposition": "currentTab" }));
        assert!(watch(NavWatch::default(), &[here]).started);
    }

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
