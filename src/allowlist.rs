//! Which CDP commands a tab channel may carry.
//!
//! The same table is enforced by the runtime for both drivers and by the
//! extension on the user's Chrome (defense in depth: `chrome.debugger` also
//! exposes `Network` and `Storage`, i.e. the tab's cookies). Anything not
//! listed is refused before it reaches the browser.

use serde_json::Value;

use crate::scripts;

const ALLOWED: &[&str] = &[
    "Page.enable",
    "Page.navigate",
    "Page.reload",
    "Page.getFrameTree",
    "Page.createIsolatedWorld",
    "Page.handleJavaScriptDialog",
    "Page.captureScreenshot",
    "Page.startScreencast",
    "Page.stopScreencast",
    "Page.screencastFrameAck",
    "Page.getNavigationHistory",
    "Page.navigateToHistoryEntry",
    "Runtime.enable",
    "Runtime.callFunctionOn",
    "Input.dispatchMouseEvent",
    "Input.dispatchKeyEvent",
    "Input.insertText",
    "DOM.getContentQuads",
    "DOM.describeNode",
    "DOM.resolveNode",
    "DOM.getNodesForSubtreeByStyle",
    "Accessibility.getFullAXTree",
    "DOMSnapshot.captureSnapshot",
    "Emulation.setFocusEmulationEnabled",
    "Emulation.setDeviceMetricsOverride",
    "Target.setAutoAttach",
];

/// `Ok(())` when `method` with `params` may be sent to a tab.
pub fn check(method: &str, params: &Value) -> Result<(), String> {
    if !ALLOWED.contains(&method) {
        return Err(format!("{method} is not on the allowlist"));
    }
    if method == "Runtime.callFunctionOn" {
        if params.get("objectId").is_some() {
            return Err("Runtime.callFunctionOn must target SenClaw's isolated world, not an object".into());
        }
        if params.get("executionContextId").and_then(Value::as_i64).is_none() {
            return Err("Runtime.callFunctionOn needs an executionContextId".into());
        }
        let declaration = params.get("functionDeclaration").and_then(Value::as_str).unwrap_or("");
        if !scripts::is_allowed(declaration) {
            return Err("Runtime.callFunctionOn with a script that is not in the approved bundle".into());
        }
    }
    if method == "Target.setAutoAttach" && params.get("flatten").and_then(Value::as_bool) != Some(true) {
        return Err("Target.setAutoAttach is allowed only with flatten: true".into());
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
}
