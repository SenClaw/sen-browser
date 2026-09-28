//! What one observation is, and the part of it the runtime keeps to decide
//! whether a later action still refers to the same page.

use serde_json::{json, Map, Value};

/// Defaults matching jev-ultrafast: visible text is capped, and so is the
/// number of candidates (a hosted choice question takes at most 255 options).
pub const DEFAULT_TEXT_CHARS: u64 = 6000;
pub const DEFAULT_MAX_CANDIDATES: u64 = 250;

/// The freshness evidence of the latest observation of a tab.
#[derive(Debug, Clone)]
pub struct Stored {
    pub id: u64,
    pub marker: Value,
    pub page_key: Value,
    pub guards: Map<String, Value>,
    pub actions: Vec<Value>,
    pub url: String,
    pub text_chars: u64,
    pub max_candidates: u64,
    /// Viewport size, for wheel scrolling at a point inside the page.
    pub w: f64,
    pub h: f64,
}

impl Stored {
    pub fn action(&self, id: &str) -> Option<&Value> {
        self.actions.iter().find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    }
}

/// Split a raw snapshot into the stored freshness evidence and the public
/// observation. Markers, page keys and guards never leave the runtime.
pub fn split(id: u64, tab_id: &str, raw: Value, text_chars: u64, max_candidates: u64, snapshot_ms: u128) -> (Stored, Value) {
    let actions = raw.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
    let url = raw.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
    let stored = Stored {
        id,
        marker: raw.get("marker").cloned().unwrap_or(Value::Null),
        page_key: raw.get("page_key").cloned().unwrap_or(Value::Null),
        guards: raw.get("guards").and_then(Value::as_object).cloned().unwrap_or_default(),
        actions: actions.clone(),
        url: url.clone(),
        text_chars,
        max_candidates,
        w: raw.get("w").and_then(Value::as_f64).unwrap_or(1120.0),
        h: raw.get("h").and_then(Value::as_f64).unwrap_or(780.0),
    };
    let public = json!({
        "observation_id": id,
        "tab_id": tab_id,
        "url": url,
        "title": raw.get("title").cloned().unwrap_or(Value::String(String::new())),
        "viewport": {
            "w": raw.get("w").cloned().unwrap_or(Value::Null),
            "h": raw.get("h").cloned().unwrap_or(Value::Null),
            "scroll_y": raw.pointer("/scroll/y").cloned().unwrap_or(Value::Null),
            "doc_h": raw.pointer("/scroll/height").cloned().unwrap_or(Value::Null),
        },
        "text": raw.get("text").cloned().unwrap_or(Value::String(String::new())),
        "actions": actions,
        "omitted_actions": raw.get("omitted_actions").cloned().unwrap_or(json!(0)),
        "coverage": raw.get("coverage").cloned().unwrap_or(json!({})),
        "dialog": Value::Null,
        "timing_ms": { "snapshot": snapshot_ms as u64 },
    });
    (stored, public)
}

/// The observation of a tab blocked by a JavaScript dialog: nothing in the
/// page can run, so the only actions are answering the dialog.
pub fn dialog_observation(id: u64, tab_id: &str, url: &str, dialog: &Value) -> (Stored, Value) {
    let actions = vec![
        json!({ "id": "dialog_accept", "kind": "dialog", "accept": true, "label": "Accept the dialog" }),
        json!({ "id": "dialog_dismiss", "kind": "dialog", "accept": false, "label": "Dismiss the dialog" }),
    ];
    let stored = Stored {
        id,
        marker: Value::Null,
        page_key: Value::Null,
        guards: Map::new(),
        actions: actions.clone(),
        url: url.to_string(),
        text_chars: DEFAULT_TEXT_CHARS,
        max_candidates: DEFAULT_MAX_CANDIDATES,
        w: 1120.0,
        h: 780.0,
    };
    let message = dialog.get("message").and_then(Value::as_str).unwrap_or_default();
    let public = json!({
        "observation_id": id,
        "tab_id": tab_id,
        "url": url,
        "title": "",
        "viewport": {},
        "text": format!("A {} dialog is open: {message}", dialog.get("type").and_then(Value::as_str).unwrap_or("JavaScript")),
        "actions": actions,
        "omitted_actions": 0,
        "coverage": {},
        "dialog": dialog,
        "timing_ms": { "snapshot": 0 },
    });
    (stored, public)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_evidence_stays_inside_the_runtime() {
        let raw = json!({
            "url": "https://a.test/", "title": "A", "w": 1120, "h": 780, "text": "hi",
            "scroll": {"y": 0, "height": 900},
            "actions": [{"id": "e1", "kind": "click", "node": 3, "label": "Go"}],
            "marker": [1, 2], "page_key": [3], "guards": {"3": [4]}, "omitted_actions": 0, "coverage": {"frames": 0}
        });
        let (stored, public) = split(7, "t1", raw, 6000, 250, 12);
        assert_eq!(stored.id, 7);
        assert_eq!(stored.guards.get("3"), Some(&json!([4])));
        assert!(stored.action("e1").is_some());
        assert!(public.get("marker").is_none() && public.get("guards").is_none() && public.get("page_key").is_none());
        assert_eq!(public["observation_id"], 7);
        assert_eq!(public["viewport"]["doc_h"], 900);
    }

    #[test]
    fn a_dialog_offers_only_answers() {
        let (stored, public) = dialog_observation(3, "t1", "https://a.test/", &json!({"type": "confirm", "message": "Sure?"}));
        assert_eq!(stored.actions.len(), 2);
        assert_eq!(public["dialog"]["type"], "confirm");
        assert!(public["text"].as_str().unwrap().contains("Sure?"));
    }
}
