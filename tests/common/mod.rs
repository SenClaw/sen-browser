//! Shared helpers for tests that drive a real Chrome.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::Value;
use sen_browser::Browser;

/// Serve `tests/fixtures` on a random loopback port; returns the base URL.
pub async fn serve_fixtures() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let app = Router::new()
        // What the browser says it is, as a page.
        .route(
            "/ua-echo",
            get(|headers: axum::http::HeaderMap| async move {
                let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                Response::builder()
                    .header("content-type", "text/html; charset=utf-8")
                    .body(Body::from(format!("<!doctype html><title>ua</title><p id=ua>{ua}</p>")))
                    .unwrap()
            }),
        )
        .route(
        "/:name",
        get(move |axum::extract::Path(name): axum::extract::Path<String>| {
            let dir = dir.clone();
            async move {
                let safe = !name.contains('/') && !name.contains("..");
                match (safe, std::fs::read_to_string(dir.join(&name))) {
                    (true, Ok(body)) => Response::builder()
                        .header("content-type", "text/html; charset=utf-8")
                        .body(Body::from(body))
                        .unwrap(),
                    _ => Response::builder().status(404).body(Body::from("not found")).unwrap(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A Browser over a throwaway data dir, or `None` when no Chrome is installed
/// (the test then passes vacuously with a note).
pub fn browser() -> Option<(Arc<Browser>, tempfile::TempDir)> {
    if sen_browser::chrome::find_chrome().is_err() {
        eprintln!("no Chrome installed; skipping a real-browser test");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    Some((Browser::new(dir.path().to_path_buf()), dir))
}

/// The action whose label contains `label` and whose kind is `kind`.
pub fn action<'a>(observation: &'a Value, kind: &str, label: &str) -> &'a Value {
    observation["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == kind && a["label"].as_str().unwrap_or_default().contains(label))
        .unwrap_or_else(|| panic!("no {kind} action labelled {label:?} in {}", observation["actions"]))
}

pub fn has_action(observation: &Value, id: &str) -> bool {
    observation["actions"].as_array().unwrap().iter().any(|a| a["id"] == id)
}

pub fn id(action: &Value) -> String {
    action["id"].as_str().unwrap().to_string()
}

pub fn obs_id(observation: &Value) -> u64 {
    observation["observation_id"].as_u64().unwrap()
}
