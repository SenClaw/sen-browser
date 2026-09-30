//! The managed driver against a real headless Chrome and a local fixture.

mod common;

use std::time::Duration;

use common::*;
use sen_browser::error::BrowserError;
use sen_browser::tab::{DriverKind, Hold};
use serde_json::Value;

async fn page_log(tab: &sen_browser::tab::Tab) -> String {
    tab.read(None).await.unwrap()["text"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observes_in_an_isolated_world_and_acts_with_trusted_input() {
    let Some((browser, _dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let session = browser.open_session(DriverKind::Managed, Some("t1"), Some(true)).await.unwrap();
    let (tab, created) = browser.open_tab(&session, None, Some("chat-a")).await.unwrap();
    assert!(created);
    let obs = tab.navigate(&format!("{base}/form.html")).await.unwrap();
    assert_eq!(obs["title"], "SenBrowser fixture");

    // What the model is offered.
    let dest = action(&obs, "fill", "Destination");
    assert_eq!(dest["value"], "Paris");
    let actions = obs["actions"].as_array().unwrap();
    assert!(!actions.iter().any(|a| a["label"].as_str().unwrap_or_default().contains("Password")), "password fields are never offered");
    let otp = action(&obs, "fill", "One-time code");
    assert_eq!(otp["sensitive"], true);
    assert_eq!((otp["value"].as_str(), otp["filled"].as_bool()), (Some(""), Some(true)), "only that it is filled");
    assert!(!obs.to_string().contains("482913"), "a sensitive value never leaves the runtime");
    action(&obs, "select", "Category → Design");
    assert!(!actions.iter().any(|a| a["label"].as_str().unwrap_or_default().contains("Hostel")), "disabled options are not offered");
    assert_eq!(action(&obs, "click", "Free cancellation")["checked"], "false");
    action(&obs, "click", "Shadow button");
    assert_eq!(action(&obs, "click", "Frame button")["frame"], true);
    assert!(obs["coverage"]["open_shadow_roots"].as_u64().unwrap() >= 1);
    assert!(obs["coverage"]["frames"].as_u64().unwrap() >= 1);
    assert!(obs["text"].as_str().unwrap().len() <= 6000);
    assert!(has_action(&obs, "scroll_down") && has_action(&obs, "wait"));
    assert!(obs.get("marker").is_none() && obs.get("guards").is_none());

    // Click: trusted.
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Search")), None).await.unwrap();
    assert_eq!(out["kind"], "click");
    assert!(page_log(&tab).await.contains("search trusted=true"));

    // Fill replaces the value, with trusted input events.
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "fill", "Destination")), Some("Lisbon")).await.unwrap();
    let obs = out["observation"].clone();
    assert_eq!(action(&obs, "fill", "Destination")["value"], "Lisbon");
    assert!(page_log(&tab).await.contains("dest=Lisbon trusted=true"));

    // Native select.
    let out = tab.act(obs_id(&obs), &id(action(&obs, "select", "Category → Design")), None).await.unwrap();
    assert!(page_log(&tab).await.contains("cat=design"));

    // Checkbox, open shadow root, same-origin frame.
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Free cancellation")), None).await.unwrap();
    let obs = out["observation"].clone();
    assert_eq!(action(&obs, "click", "Free cancellation")["checked"], "true");
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Shadow button")), None).await.unwrap();
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Frame button")), None).await.unwrap();
    let log = page_log(&tab).await;
    assert!(log.contains("free=true") && log.contains("shadow trusted=true") && log.contains("frame trusted=true"), "{log}");

    // Enter in a field with no form and no button.
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "fill", "Query")), Some("hello")).await.unwrap();
    let obs = out["observation"].clone();
    assert!(has_action(&obs, "key_enter"), "a focused field offers Enter");
    let out = tab.act(obs_id(&obs), "key_enter", None).await.unwrap();
    assert!(page_log(&tab).await.contains("enter trusted=true q=hello"));

    // Scroll and wait.
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), "scroll_down", None).await.unwrap();
    let obs = out["observation"].clone();
    assert!(obs["viewport"]["scroll_y"].as_f64().unwrap() > 0.0);
    assert!(has_action(&obs, "scroll_up"));
    let out = tab.act(obs_id(&obs), "wait", None).await.unwrap();
    assert_eq!(out["kind"], "wait");

    // The page cannot see SenClaw's node cache.
    let obs = tab.act(obs_id(&out["observation"]), "scroll_up", None).await.unwrap()["observation"].clone();
    tab.act(obs_id(&obs), &id(action(&obs, "click", "Check isolation")), None).await.unwrap();
    assert!(page_log(&tab).await.contains("senclaw=undefined"));

    // A credential-like field is never typed into.
    let obs = tab.observe(None, None).await.unwrap();
    let err = tab.act(obs_id(&obs), &id(action(&obs, "fill", "One-time code")), Some("123456")).await.unwrap_err();
    assert!(matches!(err, BrowserError::Blocked(_)), "{err}");

    // The owner gets the same tab back.
    let (again, created) = browser.open_tab(&session, None, Some("chat-a")).await.unwrap();
    assert!(!created);
    assert_eq!(again.id, tab.id);
    browser.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_and_covered_targets_are_refused_before_any_input() {
    let Some((browser, _dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let session = browser.open_session(DriverKind::Managed, Some("t2"), Some(true)).await.unwrap();
    let (tab, _) = browser.open_tab(&session, None, Some("chat-b")).await.unwrap();
    let obs = tab.navigate(&format!("{base}/form.html")).await.unwrap();

    // Covered: hit-testing finds the overlay, nothing is clicked.
    let err = tab.act(obs_id(&obs), &id(action(&obs, "click", "Covered button")), None).await.unwrap_err();
    assert!(matches!(err, BrowserError::TargetCovered(_)), "{err}");
    assert!(!page_log(&tab).await.contains("covered clicked"));

    // A consumed or superseded observation cannot be acted on.
    let obs = tab.observe(None, None).await.unwrap();
    let old = obs_id(&obs);
    let _newer = tab.observe(None, None).await.unwrap();
    let err = tab.act(old, &id(action(&obs, "click", "Search")), None).await.unwrap_err();
    assert!(matches!(err, BrowserError::StalePage), "{err}");

    // The target changes by itself between observation and action: refused.
    let obs = tab.observe(None, None).await.unwrap();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Arm change")), None).await.unwrap();
    let obs = out["observation"].clone();
    let target = id(action(&obs, "click", "Target"));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let err = tab.act(obs_id(&obs), &target, None).await.unwrap_err();
    assert!(matches!(err, BrowserError::StalePage), "{err}");
    assert!(!page_log(&tab).await.contains("target clicked"), "no input after a stale guard");

    // Observing again gives a clickable target.
    let obs = tab.observe(None, None).await.unwrap();
    tab.act(obs_id(&obs), &id(action(&obs, "click", "Target (changed)")), None).await.unwrap();
    assert!(page_log(&tab).await.contains("target clicked"));
    browser.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combobox_suggestions_and_dialogs() {
    let Some((browser, _dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let session = browser.open_session(DriverKind::Managed, Some("t3"), Some(true)).await.unwrap();
    let (tab, _) = browser.open_tab(&session, None, Some("chat-c")).await.unwrap();
    let obs = tab.navigate(&format!("{base}/form.html")).await.unwrap();

    // Typing into a combobox waits for its suggestions before observing.
    let out = tab.act(obs_id(&obs), &id(action(&obs, "fill", "City")), Some("Zur")).await.unwrap();
    let obs = out["observation"].clone();
    let option = action(&obs, "click", "Zürich, Switzerland");
    assert_eq!(option["role"], "option");
    let out = tab.act(obs_id(&obs), &id(option), None).await.unwrap();
    assert!(page_log(&tab).await.contains("city=Zürich, Switzerland"));

    // A confirm() dialog blocks the page; the only actions answer it.
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Confirm")), None).await.unwrap();
    let obs = out["observation"].clone();
    assert_eq!(obs["dialog"]["type"], "confirm", "{obs}");
    let ids: Vec<&str> = obs["actions"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["dialog_accept", "dialog_dismiss"]);
    let out = tab.act(obs_id(&obs), "dialog_dismiss", None).await.unwrap();
    assert!(out["observation"]["dialog"].is_null());
    assert!(page_log(&tab).await.contains("confirm=false"));
    browser.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigate_read_screenshot_back_and_handover_gate() {
    let Some((browser, _dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let session = browser.open_session(DriverKind::Managed, Some("t4"), Some(true)).await.unwrap();
    let (tab, _) = browser.open_tab(&session, None, Some("chat-d")).await.unwrap();
    tab.navigate(&format!("{base}/form.html")).await.unwrap();

    let read = tab.read(Some(5000)).await.unwrap();
    assert!(read["text"].as_str().unwrap().contains("Fixture"));
    assert!(read["links"].as_array().unwrap().iter().any(|l| l["text"] == "Other page"));
    let shot = tab.screenshot().await.unwrap();
    assert!(shot["data"].as_str().unwrap().starts_with("/9j/"), "a JPEG");

    // Follow a link, then go back.
    let obs = tab.observe(None, None).await.unwrap();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Other page")), None).await.unwrap();
    let mut obs = out["observation"].clone();
    if obs.is_null() {
        obs = tab.observe(None, None).await.unwrap();
    }
    assert_eq!(obs["title"], "Other page");
    let out = tab.act(obs_id(&obs), "go_back", None).await.unwrap();
    let mut obs = out["observation"].clone();
    if obs.is_null() {
        obs = tab.observe(None, None).await.unwrap();
    }
    assert_eq!(obs["title"], "SenBrowser fixture");

    // Only web pages open.
    assert!(matches!(tab.navigate("file:///etc/passwd").await.unwrap_err(), BrowserError::Blocked(_)));

    // While handed over, the agent can neither read nor act.
    tab.set_hold(Hold::Handover).await;
    assert!(matches!(tab.observe(None, None).await.unwrap_err(), BrowserError::HandoverActive));
    assert!(matches!(tab.read(None).await.unwrap_err(), BrowserError::HandoverActive));
    assert!(matches!(tab.screenshot().await.unwrap_err(), BrowserError::HandoverActive));
    assert!(matches!(tab.act(1, "wait", None).await.unwrap_err(), BrowserError::HandoverActive));
    let back: Value = browser.handover_end(&tab.id).await.unwrap();
    assert_eq!(back["hold"], "none");
    tab.observe(None, None).await.unwrap();
    browser.shutdown().await;
}

/// A headless page is told the browser is an ordinary Chrome: sites answer
/// "HeadlessChrome" with a bot wall (DuckDuckGo's challenge on every search).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_headless_page_sees_an_ordinary_chrome() {
    let Some((browser, _dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let session = browser.open_session(DriverKind::Managed, Some("ua"), Some(true)).await.unwrap();
    let (tab, _) = browser.open_tab(&session, None, Some("chat-ua")).await.unwrap();
    tab.navigate(&format!("{base}/ua-echo")).await.unwrap();
    let text = page_log(&tab).await;
    assert!(text.contains("Chrome/") && !text.contains("Headless"), "{text}");
}

/// A toggle button (like, follow, mute) reports whether it is on, and a
/// toggle that changed by itself since it was observed is not clicked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_toggle_button_says_whether_it_is_on() {
    let Some((browser, _dir)) = browser() else { return };
    let base = serve_fixtures().await;
    let session = browser.open_session(DriverKind::Managed, Some("toggle"), Some(true)).await.unwrap();
    let (tab, _) = browser.open_tab(&session, None, Some("chat-toggle")).await.unwrap();
    let obs = tab.navigate(&format!("{base}/feed.html")).await.unwrap();
    assert_eq!(action(&obs, "click", "Like first post")["checked"], "false");

    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Like first post")), None).await.unwrap();
    let obs = out["observation"].clone();
    assert_eq!(action(&obs, "click", "Like first post")["checked"], "true");

    // Turn it off, then let the page turn it on behind the observation.
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Like first post")), None).await.unwrap();
    let obs = out["observation"].clone();
    let out = tab.act(obs_id(&obs), &id(action(&obs, "click", "Flip the like")), None).await.unwrap();
    let obs = out["observation"].clone();
    assert_eq!(action(&obs, "click", "Like first post")["checked"], "false");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let err = tab.act(obs_id(&obs), &id(action(&obs, "click", "Like first post")), None).await.unwrap_err();
    assert!(matches!(err, BrowserError::StalePage), "a like that is already on must not be clicked off: {err}");
    browser.shutdown().await;
}
