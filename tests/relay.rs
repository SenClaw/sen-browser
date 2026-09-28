//! The extension driver end to end: a fake extension speaking the `drv`
//! protocol over the runtime's relay endpoint, backed by a real Chrome.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use sen_browser::chrome::{self, ManagedChrome};
use sen_browser::error::BrowserError;
use sen_browser::tab::DriverKind;
use sen_browser::{allowlist, http, scripts};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

struct FakeExtension {
    out: mpsc::UnboundedSender<String>,
    tabs: Arc<Mutex<HashMap<i64, String>>>,
    chrome: Arc<ManagedChrome>,
    inbox: Arc<Mutex<Vec<Value>>>,
}

impl FakeExtension {
    /// Connect to `ws_url`, say hello with `scripts_hash`, and serve requests
    /// the way the real extension does (allowlist included).
    async fn connect(ws_url: &str, chrome: Arc<ManagedChrome>, scripts_hash: &str) -> FakeExtension {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if sink.send(Message::Text(frame)).await.is_err() {
                    break;
                }
            }
        });
        let tabs: Arc<Mutex<HashMap<i64, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let inbox = Arc::new(Mutex::new(Vec::new()));

        // Forward CDP events of mapped tabs.
        {
            let tabs = tabs.clone();
            let out = out.clone();
            let mut events = chrome.conn.subscribe();
            tokio::spawn(async move {
                while let Ok(ev) = events.recv().await {
                    let Some(session) = ev.session.clone() else { continue };
                    let tab = tabs.lock().await.iter().find(|(_, s)| **s == session).map(|(t, _)| *t);
                    if let Some(tab) = tab {
                        let _ = out.send(json!({ "ch": "drv", "t": "event", "tab": tab, "method": ev.method, "params": ev.params }).to_string());
                    }
                }
            });
        }

        // Serve requests.
        {
            let tabs = tabs.clone();
            let out = out.clone();
            let chrome = chrome.clone();
            let inbox = inbox.clone();
            tokio::spawn(async move {
                let mut next_tab = 1000i64;
                while let Some(Ok(Message::Text(text))) = stream.next().await {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    inbox.lock().await.push(req.clone());
                    let Some(id) = req.get("id").cloned() else { continue };
                    let reply = move |body: Value| {
                        let mut frame = json!({ "ch": "drv", "id": id });
                        for (k, v) in body.as_object().unwrap() {
                            frame[k] = v.clone();
                        }
                        frame.to_string()
                    };
                    match req["t"].as_str().unwrap_or_default() {
                        "open_tab" => {
                            let (_, sid) = chrome.open_target(req["url"].as_str().unwrap_or("about:blank")).await.unwrap();
                            next_tab += 1;
                            tabs.lock().await.insert(next_tab, sid);
                            let _ = out.send(reply(json!({ "ok": { "tab": next_tab } })));
                        }
                        "attach" | "detach" | "focus" | "close_tab" => {
                            let _ = out.send(reply(json!({ "ok": {} })));
                        }
                        "cdp" => {
                            let tab = req["tab"].as_i64().unwrap();
                            let method = req["method"].as_str().unwrap().to_string();
                            let params = req["params"].clone();
                            if let Err(message) = allowlist::check(&method, &params) {
                                let _ = out.send(reply(json!({ "err": "blocked_method", "message": message })));
                                continue;
                            }
                            let sid = tabs.lock().await.get(&tab).cloned();
                            let chrome = chrome.clone();
                            let out = out.clone();
                            tokio::spawn(async move {
                                let Some(sid) = sid else {
                                    let _ = out.send(reply(json!({ "err": "not_owned", "message": "unknown tab" })));
                                    return;
                                };
                                let answer = match chrome.conn.call(&method, params, Some(&sid)).await {
                                    Ok(r) => json!({ "ok": r }),
                                    Err(e) => json!({ "err": "cdp_error", "message": e.to_string() }),
                                };
                                let _ = out.send(reply(answer));
                            });
                        }
                        _ => {}
                    }
                }
            });
        }

        let hello = json!({ "ch": "drv", "t": "hello", "ext": "fake", "v": "0.2.0", "scripts": scripts_hash, "chrome": "test" });
        out.send(hello.to_string()).unwrap();
        FakeExtension { out, tabs, chrome, inbox }
    }

    fn send(&self, frame: Value) {
        self.out.send(frame.to_string()).unwrap();
    }
}

async fn start_runtime(browser: Arc<sen_browser::Browser>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, http::router(browser)).await.unwrap();
    });
    format!("ws://{addr}/v1/drivers/extension")
}

async fn wait_for_relay(browser: &sen_browser::Browser) {
    for _ in 0..100 {
        if browser.relay().await.and_then(|r| Some(r)).is_some() {
            if let Some(r) = browser.relay().await {
                if r.hello().await.is_some() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the relay never connected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_extension_driver_observes_and_acts_through_the_relay() {
    let Some((browser, dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let ws_url = start_runtime(browser.clone()).await;

    // The "person's Chrome": a separate Chrome the fake extension drives.
    let binary = chrome::find_chrome().unwrap();
    let their_chrome = Arc::new(
        ManagedChrome::launch(&binary, &dir.path().join("their-chrome"), true, &dir.path().join("their-pids.json"))
            .await
            .unwrap(),
    );
    let ext = FakeExtension::connect(&ws_url, their_chrome.clone(), &scripts::bundle_hash()).await;
    wait_for_relay(&browser).await;

    let session = browser.open_session(DriverKind::Extension, None, None).await.unwrap();
    let (tab, _) = browser.open_tab(&session, None, Some("chat-x")).await.unwrap();
    assert_eq!(tab.driver, DriverKind::Extension);
    let obs = tab.navigate(&format!("{base}/form.html")).await.unwrap();
    assert_eq!(obs["title"], "SenBrowser fixture");
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Search")), None).await.unwrap();
    assert!(tab.read(None).await.unwrap()["text"].as_str().unwrap().contains("search trusted=true"));
    let obs = out["observation"].clone();
    tab.act(obs_id(&obs), &id(action(&obs, "fill", "Destination")), Some("Hà Nội")).await.unwrap();
    assert!(tab.read(None).await.unwrap()["text"].as_str().unwrap().contains("dest=Hà Nội trusted=true"));

    // The extension enforces the allowlist too.
    let relay = browser.relay().await.unwrap();
    let ext_tab: i64 = tab_target(&tab).await;
    let blocked = relay.request("cdp", json!({ "tab": ext_tab, "method": "Network.getCookies", "params": {} })).await;
    assert!(matches!(blocked, Err(sen_browser::cdp::CdpError::Blocked(_))), "{blocked:?}");

    // The person cancels debugging from Chrome's bar: the agent stops.
    ext.send(json!({ "ch": "drv", "t": "detached", "tab": ext_tab, "reason": "canceled_by_user" }));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(tab.observe(None, None).await.unwrap_err(), BrowserError::Detached));
    browser.handover_end(&tab.id).await.unwrap();
    assert!(ext.inbox.lock().await.iter().any(|f| f["t"] == "attach" && f["tab"] == ext_tab), "resuming re-attaches");
    tab.observe(None, None).await.unwrap();

    // The person clicks into the agent's tab: the agent yields.
    ext.send(json!({ "ch": "drv", "t": "user_active", "tab": ext_tab }));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(tab.observe(None, None).await.unwrap_err(), BrowserError::UserActive));
    browser.handover_end(&tab.id).await.unwrap();
    tab.observe(None, None).await.unwrap();

    // Handover detaches first and focuses the tab.
    browser.handover_start(&tab.id).await.unwrap();
    {
        let inbox = ext.inbox.lock().await;
        let detach = inbox.iter().rposition(|f| f["t"] == "detach" && f["tab"] == ext_tab).expect("detached");
        let focus = inbox.iter().rposition(|f| f["t"] == "focus" && f["tab"] == ext_tab).expect("focused");
        assert!(detach < focus, "nothing can read the page once the person has it");
    }
    assert!(matches!(tab.read(None).await.unwrap_err(), BrowserError::HandoverActive));
    browser.handover_end(&tab.id).await.unwrap();

    // A tab the person shares from the side panel can be adopted.
    let (_, sid) = ext.chrome.open_target(&format!("{base}/other.html")).await.unwrap();
    ext.tabs.lock().await.insert(4242, sid);
    ext.send(json!({ "ch": "drv", "t": "shared_tab", "tab": 4242, "url": format!("{base}/other.html"), "title": "Other page" }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shared = browser.adopt_shared(&session, 4242, Some("chat-y")).await.unwrap();
    let obs = shared.observe(None, None).await.unwrap();
    assert_eq!(obs["title"], "Other page");

    // The person closes the agent's tab.
    ext.send(json!({ "ch": "drv", "t": "tab_closed", "tab": ext_tab }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(tab.observe(None, None).await.unwrap_err(), BrowserError::NotFound(_)));

    browser.shutdown().await;
    their_chrome.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_extension_built_for_other_scripts_is_rejected() {
    let Some((browser, dir)) = browser() else { return };
    let ws_url = start_runtime(browser.clone()).await;
    let binary = chrome::find_chrome().unwrap();
    let their_chrome = Arc::new(
        ManagedChrome::launch(&binary, &dir.path().join("their-chrome"), true, &dir.path().join("their-pids.json"))
            .await
            .unwrap(),
    );
    let ext = FakeExtension::connect(&ws_url, their_chrome.clone(), "sha256:not-this-bundle").await;
    let mut rejected = false;
    for _ in 0..100 {
        if ext.inbox.lock().await.iter().any(|f| f["t"] == "reject" && f["code"] == "scripts_mismatch") {
            rejected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rejected, "the runtime refuses an extension with another script bundle");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(browser.relay().await.is_none());
    assert!(matches!(
        browser.open_session(DriverKind::Extension, None, None).await.err(),
        Some(BrowserError::ExtensionNotConnected)
    ));
    their_chrome.shutdown().await;
}

async fn tab_target(tab: &sen_browser::tab::Tab) -> i64 {
    tab.target().await.parse().unwrap()
}
