//! Sessions and tabs across both drivers.
//!
//! A session is one browser the agent drives: a managed Chrome with a named
//! profile, or the person's Chrome through the extension. Sessions are keyed
//! by `(driver, profile)` so repeated tasks reuse the same browser (and its
//! sign-ins); each owner (a chat) gets one tab per session, reused like the
//! old extension's one-tab-per-agent group.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use crate::cdp::{SessionChannel, TabChannel};
use crate::chrome::{self, ManagedChrome};
use crate::error::{BrowserError, Result};
use crate::relay::{ExtTabChannel, RelayConn};
use crate::scripts;
use crate::tab::{DriverKind, Hold, Tab};

pub struct Session {
    pub id: String,
    pub driver: DriverKind,
    pub profile: String,
    chrome: Mutex<Option<ManagedChrome>>,
    relay: Option<Arc<RelayConn>>,
    tabs: Mutex<Vec<Arc<Tab>>>,
}

impl Session {
    pub async fn describe(&self) -> Value {
        let mut tabs = Vec::new();
        for t in self.tabs.lock().await.iter() {
            tabs.push(t.describe().await);
        }
        let headless = self.chrome.lock().await.as_ref().map(|c| c.headless);
        json!({ "id": self.id, "driver": self.driver, "profile": self.profile, "headless": headless, "tabs": tabs })
    }
}

pub struct Browser {
    data_dir: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tabs: RwLock<HashMap<String, (Arc<Tab>, String)>>,
    relay: RwLock<Option<Arc<RelayConn>>>,
    seq: AtomicU64,
    epoch: String,
}

impl Browser {
    pub fn new(data_dir: PathBuf) -> Arc<Browser> {
        let _ = std::fs::create_dir_all(&data_dir);
        chrome::reap_orphans(&data_dir.join("chrome-pids.json"));
        Arc::new(Browser {
            data_dir,
            sessions: Mutex::new(HashMap::new()),
            tabs: RwLock::new(HashMap::new()),
            relay: RwLock::new(None),
            seq: AtomicU64::new(1),
            epoch: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
        })
    }

    /// Ids carry this process's epoch: after a restart a caller's remembered
    /// `t2` must name nothing, never the new process's `t2` (another chat's tab).
    fn next(&self, prefix: &str) -> String {
        format!("{prefix}{}-{}", self.epoch, self.seq.fetch_add(1, Ordering::SeqCst))
    }

    fn pids_file(&self) -> PathBuf {
        self.data_dir.join("chrome-pids.json")
    }

    // ----- extension relay -----

    pub async fn set_relay(&self, relay: Arc<RelayConn>) {
        if let Some(old) = self.relay.write().await.replace(relay) {
            old.close().await;
        }
    }

    pub async fn drop_relay(&self, relay: &Arc<RelayConn>) {
        let mut current = self.relay.write().await;
        if current.as_ref().map(|r| Arc::ptr_eq(r, relay)).unwrap_or(false) {
            *current = None;
        }
        relay.close().await;
        // Sessions on this relay cannot do anything any more.
        let mut sessions = self.sessions.lock().await;
        let dead: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.relay.as_ref().map(|r| Arc::ptr_eq(r, relay)).unwrap_or(false))
            .map(|(k, _)| k.clone())
            .collect();
        for key in dead {
            if let Some(s) = sessions.remove(&key) {
                self.forget_tabs(&s).await;
            }
        }
    }

    pub async fn relay(&self) -> Option<Arc<RelayConn>> {
        self.relay.read().await.clone().filter(|r| !r.is_closed())
    }

    // ----- sessions -----

    pub async fn open_session(&self, driver: DriverKind, profile: Option<&str>, headless: Option<bool>) -> Result<Arc<Session>> {
        let profile = profile.unwrap_or("default").to_string();
        let key = format!("{driver:?}:{profile}");
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(&key) {
            let alive = match s.driver {
                DriverKind::Managed => s.chrome.lock().await.as_ref().map(|c| !c.conn.is_closed()).unwrap_or(false),
                DriverKind::Extension => s.relay.as_ref().map(|r| !r.is_closed()).unwrap_or(false),
            };
            if alive {
                return Ok(s.clone());
            }
            let stale = sessions.remove(&key).expect("present");
            self.forget_tabs(&stale).await;
        }
        let session = match driver {
            DriverKind::Managed => {
                let binary = chrome::find_chrome()?;
                let dir = chrome::profile_dir(&self.data_dir, &profile)?;
                let headless = headless.unwrap_or_else(|| std::env::var("SEN_BROWSER_HEADFUL").map(|v| v != "1").unwrap_or(true));
                let managed = ManagedChrome::launch(&binary, &dir, headless, &self.pids_file()).await?;
                Session {
                    id: self.next("s"),
                    driver,
                    profile: profile.clone(),
                    chrome: Mutex::new(Some(managed)),
                    relay: None,
                    tabs: Mutex::new(Vec::new()),
                }
            }
            DriverKind::Extension => {
                let relay = self.relay().await.ok_or(BrowserError::ExtensionNotConnected)?;
                Session {
                    id: self.next("s"),
                    driver,
                    profile: profile.clone(),
                    chrome: Mutex::new(None),
                    relay: Some(relay),
                    tabs: Mutex::new(Vec::new()),
                }
            }
        };
        let session = Arc::new(session);
        sessions.insert(key, session.clone());
        Ok(session)
    }

    pub async fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions
            .lock()
            .await
            .values()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| BrowserError::NotFound(format!("session {id}")))
    }

    pub async fn sessions(&self) -> Vec<Value> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().await.values().cloned().collect();
        let mut out = Vec::new();
        for s in sessions {
            out.push(s.describe().await);
        }
        out
    }

    pub async fn close_session(&self, id: &str) -> Result<()> {
        let session = {
            let mut sessions = self.sessions.lock().await;
            let key = sessions
                .iter()
                .find(|(_, s)| s.id == id)
                .map(|(k, _)| k.clone())
                .ok_or_else(|| BrowserError::NotFound(format!("session {id}")))?;
            sessions.remove(&key).expect("present")
        };
        if let Some(relay) = &session.relay {
            for tab in session.tabs.lock().await.iter() {
                if let Ok(ext) = tab.target().await.parse::<i64>() {
                    let _ = relay.request("detach", json!({ "tab": ext })).await;
                }
            }
        }
        self.forget_tabs(&session).await;
        if let Some(chrome) = session.chrome.lock().await.take() {
            chrome.shutdown().await;
        }
        Ok(())
    }

    async fn forget_tabs(&self, session: &Session) {
        let tabs: Vec<Arc<Tab>> = session.tabs.lock().await.drain(..).collect();
        let mut index = self.tabs.write().await;
        for t in tabs {
            t.close().await;
            index.remove(&t.id);
        }
    }

    // ----- tabs -----

    /// The owner's tab in this session (navigated to `url` when given), or a new one.
    pub async fn open_tab(&self, session: &Arc<Session>, url: Option<&str>, owner: Option<&str>) -> Result<(Arc<Tab>, bool)> {
        if let Some(url) = url {
            crate::tab::check_url(url)?;
        }
        if let Some(owner) = owner {
            let existing = session
                .tabs
                .lock()
                .await
                .iter()
                .find(|t| t.owner.as_deref() == Some(owner) && !t.is_closed())
                .cloned();
            if let Some(tab) = existing {
                return Ok((tab, false));
            }
        }
        let id = self.next("t");
        let tab = match session.driver {
            DriverKind::Managed => {
                let guard = session.chrome.lock().await;
                let chrome = guard.as_ref().ok_or_else(|| BrowserError::Internal("the session's Chrome is gone".into()))?;
                let (target, sid) = chrome.open_target("about:blank").await?;
                let channel: Arc<dyn TabChannel> = Arc::new(SessionChannel { conn: chrome.conn.clone(), session: sid });
                Tab::attach(id, owner.map(str::to_string), DriverKind::Managed, target, channel, true).await?
            }
            DriverKind::Extension => {
                let relay = session.relay.clone().ok_or(BrowserError::ExtensionNotConnected)?;
                let opened = relay.request("open_tab", json!({ "url": "about:blank", "owner": owner })).await?;
                let ext = opened
                    .get("tab")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| BrowserError::Internal("the extension opened no tab".into()))?;
                relay.request("attach", json!({ "tab": ext })).await?;
                let channel: Arc<dyn TabChannel> = Arc::new(ExtTabChannel { relay, tab: ext });
                Tab::attach(id, owner.map(str::to_string), DriverKind::Extension, ext.to_string(), channel, false).await?
            }
        };
        session.tabs.lock().await.push(tab.clone());
        self.tabs.write().await.insert(tab.id.clone(), (tab.clone(), session.id.clone()));
        Ok((tab, true))
    }

    /// Adopt a tab the person shared from the side panel.
    pub async fn adopt_shared(&self, session: &Arc<Session>, ext_tab: i64, owner: Option<&str>) -> Result<Arc<Tab>> {
        let relay = session.relay.clone().ok_or_else(|| BrowserError::BadRequest("only an extension session has shared tabs".into()))?;
        relay
            .take_shared(ext_tab)
            .await
            .ok_or_else(|| BrowserError::NotFound(format!("the person has not shared tab {ext_tab}")))?;
        relay.request("attach", json!({ "tab": ext_tab })).await?;
        let channel: Arc<dyn TabChannel> = Arc::new(ExtTabChannel { relay, tab: ext_tab });
        let tab = Tab::attach(self.next("t"), owner.map(str::to_string), DriverKind::Extension, ext_tab.to_string(), channel, false).await?;
        session.tabs.lock().await.push(tab.clone());
        self.tabs.write().await.insert(tab.id.clone(), (tab.clone(), session.id.clone()));
        Ok(tab)
    }

    pub async fn tab(&self, id: &str) -> Result<Arc<Tab>> {
        self.tabs
            .read()
            .await
            .get(id)
            .map(|(t, _)| t.clone())
            .ok_or_else(|| BrowserError::NotFound(format!("tab {id}")))
    }

    async fn session_of(&self, tab_id: &str) -> Result<Arc<Session>> {
        let sid = self
            .tabs
            .read()
            .await
            .get(tab_id)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| BrowserError::NotFound(format!("tab {tab_id}")))?;
        self.session(&sid).await
    }

    pub async fn close_tab(&self, id: &str) -> Result<()> {
        let tab = self.tab(id).await?;
        let session = self.session_of(id).await?;
        match session.driver {
            DriverKind::Managed => {
                if let Some(chrome) = session.chrome.lock().await.as_ref() {
                    chrome.close_target(&tab.target().await).await;
                }
            }
            DriverKind::Extension => {
                if let (Some(relay), Ok(ext)) = (&session.relay, tab.target().await.parse::<i64>()) {
                    let _ = relay.request("detach", json!({ "tab": ext })).await;
                }
            }
        }
        tab.close().await;
        session.tabs.lock().await.retain(|t| t.id != id);
        self.tabs.write().await.remove(id);
        Ok(())
    }

    // ----- handover -----

    /// Give the tab to the person. Managed: Chrome comes back with a window
    /// (same profile, so the sign-in sticks). Extension: the debugger detaches
    /// first — nothing can read the page while they type — and the tab is
    /// brought to the front.
    pub async fn handover_start(&self, tab_id: &str) -> Result<Value> {
        let tab = self.tab(tab_id).await?;
        let session = self.session_of(tab_id).await?;
        match session.driver {
            DriverKind::Managed => {
                let headless = session.chrome.lock().await.as_ref().map(|c| c.headless).unwrap_or(false);
                if headless {
                    self.relaunch(&session, false).await?;
                }
            }
            DriverKind::Extension => {
                let relay = session.relay.clone().ok_or(BrowserError::ExtensionNotConnected)?;
                let ext: i64 = tab.target().await.parse().map_err(|_| BrowserError::Internal("bad extension tab id".into()))?;
                relay.request("detach", json!({ "tab": ext })).await?;
                relay.request("focus", json!({ "tab": ext })).await?;
            }
        }
        tab.set_hold(Hold::Handover).await;
        Ok(tab.describe().await)
    }

    /// Take the tab back (after a handover, the person's own use of the tab,
    /// or their cancelling the debugger), then prepare it again.
    pub async fn handover_end(&self, tab_id: &str) -> Result<Value> {
        let tab = self.tab(tab_id).await?;
        let session = self.session_of(tab_id).await?;
        let hold = tab.hold().await;
        if session.driver == DriverKind::Extension && matches!(hold, Hold::Handover | Hold::Detached) {
            let relay = session.relay.clone().ok_or(BrowserError::ExtensionNotConnected)?;
            let ext: i64 = tab.target().await.parse().map_err(|_| BrowserError::Internal("bad extension tab id".into()))?;
            relay.request("attach", json!({ "tab": ext })).await?;
            tab.reprepare().await?;
        }
        tab.set_hold(Hold::None).await;
        Ok(tab.describe().await)
    }

    /// Restart a managed session's Chrome with or without a window, keeping
    /// every tab's id and URL.
    async fn relaunch(&self, session: &Arc<Session>, headless: bool) -> Result<()> {
        let mut guard = session.chrome.lock().await;
        let old = guard.take().ok_or_else(|| BrowserError::Internal("the session's Chrome is gone".into()))?;
        let profile_dir = old.profile_dir.clone();
        old.shutdown().await;
        let binary = chrome::find_chrome()?;
        let fresh = ManagedChrome::launch(&binary, &profile_dir, headless, &self.pids_file()).await?;
        for tab in session.tabs.lock().await.iter() {
            let url = tab.last_url().await;
            let url = if url.is_empty() { "about:blank".to_string() } else { url };
            let (target, sid) = fresh.open_target(&url).await?;
            let channel: Arc<dyn TabChannel> = Arc::new(SessionChannel { conn: fresh.conn.clone(), session: sid });
            tab.rebind(target, channel).await?;
        }
        *guard = Some(fresh);
        Ok(())
    }

    pub async fn status(&self) -> Value {
        let relay = self.relay().await;
        let hello = match &relay {
            Some(r) => r.hello().await,
            None => None,
        };
        let shared = match &relay {
            Some(r) => r.shared_tabs().await,
            None => Vec::new(),
        };
        json!({
            "chrome": chrome::find_chrome().map(|p| p.display().to_string()).ok(),
            "extension": { "connected": relay.is_some(), "hello": hello, "shared_tabs": shared },
            "scripts": scripts::bundle_hash(),
            "sessions": self.sessions().await,
        })
    }

    /// Close every session (runtime shutdown).
    pub async fn shutdown(&self) {
        let ids: Vec<String> = self.sessions.lock().await.values().map(|s| s.id.clone()).collect();
        for id in ids {
            let _ = self.close_session(&id).await;
        }
    }
}
