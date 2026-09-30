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
        // A 1×1 image that arrives `ms` late: what keeps a page's load event
        // waiting while everything a person would use is already there.
        .route(
            "/slow-pixel",
            get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, u64>>| async move {
                tokio::time::sleep(std::time::Duration::from_millis(q.get("ms").copied().unwrap_or(0).min(30_000))).await;
                const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x01D\x00;";
                Response::builder().header("content-type", "image/gif").body(Body::from(GIF)).unwrap()
            }),
        )
        // The script of an app shell: what the page shows arrives `ms` after it ran.
        .route(
            "/late.js",
            get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, u64>>| async move {
                let ms = q.get("ms").copied().unwrap_or(0).min(30_000);
                let js = format!(
                    "setTimeout(() => {{ document.getElementById('more').innerHTML = \
                     '<article><p>Second post</p><button>Like second post</button></article>'; }}, {ms});"
                );
                Response::builder().header("content-type", "text/javascript").body(Body::from(js)).unwrap()
            }),
        )
        // A page whose server answers `ms` late.
        .route(
            "/slow-page",
            get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, u64>>| async move {
                tokio::time::sleep(std::time::Duration::from_millis(q.get("ms").copied().unwrap_or(0).min(30_000))).await;
                Response::builder()
                    .header("content-type", "text/html; charset=utf-8")
                    .body(Body::from("<!doctype html><title>Slow page</title><h1>Slow page</h1><p>It took its time.</p>"))
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
