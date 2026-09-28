//! The runtime's HTTP API, mounted inside the SDK scaffold (which adds
//! `/health`, `/runtime/info`, `/runtime/shutdown` and bearer auth).

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::browser::Browser;
use crate::error::Result;
use crate::relay::RelayConn;
use crate::scripts;
use crate::tab::DriverKind;

type AppState = Arc<Browser>;

pub fn router(browser: Arc<Browser>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/scripts", get(script_manifest))
        .route("/v1/sessions", get(list_sessions).post(open_session))
        .route("/v1/sessions/:sid", delete(close_session))
        .route("/v1/sessions/:sid/tabs", get(list_tabs).post(open_tab))
        .route("/v1/tabs/:tid", delete(close_tab))
        .route("/v1/tabs/:tid/observe", post(observe))
        .route("/v1/tabs/:tid/act", post(act))
        .route("/v1/tabs/:tid/navigate", post(navigate))
        .route("/v1/tabs/:tid/read", post(read))
        .route("/v1/tabs/:tid/screenshot", post(screenshot))
        .route("/v1/tabs/:tid/handover", post(handover_start).delete(handover_end))
        .route("/v1/drivers/extension", get(extension_driver))
        .with_state(browser)
}

async fn status(State(b): State<AppState>) -> Json<Value> {
    Json(b.status().await)
}

async fn script_manifest() -> Json<Value> {
    Json(scripts::manifest())
}

async fn list_sessions(State(b): State<AppState>) -> Json<Value> {
    Json(json!({ "sessions": b.sessions().await }))
}

#[derive(Deserialize)]
struct OpenSession {
    driver: DriverKind,
    profile: Option<String>,
    headless: Option<bool>,
}

async fn open_session(State(b): State<AppState>, Json(req): Json<OpenSession>) -> Result<Json<Value>> {
    let s = b.open_session(req.driver, req.profile.as_deref(), req.headless).await?;
    Ok(Json(s.describe().await))
}

async fn close_session(State(b): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    b.close_session(&sid).await?;
    Ok(Json(json!({ "closed": sid })))
}

async fn list_tabs(State(b): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    let s = b.session(&sid).await?;
    let mut described = s.describe().await;
    if s.driver == DriverKind::Extension {
        if let Some(relay) = b.relay().await {
            described["shared_tabs"] = Value::Array(relay.shared_tabs().await);
        }
    }
    Ok(Json(described))
}

#[derive(Deserialize)]
struct OpenTab {
    url: Option<String>,
    owner: Option<String>,
    /// Adopt a tab the person shared from the extension's side panel.
    ext_tab: Option<i64>,
}

async fn open_tab(State(b): State<AppState>, Path(sid): Path<String>, Json(req): Json<OpenTab>) -> Result<Json<Value>> {
    let s = b.session(&sid).await?;
    let (tab, created) = match req.ext_tab {
        Some(ext) => (b.adopt_shared(&s, ext, req.owner.as_deref()).await?, true),
        None => b.open_tab(&s, req.url.as_deref(), req.owner.as_deref()).await?,
    };
    let observation = match req.url.as_deref() {
        Some(url) => tab.navigate(url).await?,
        None => tab.observe(None, None).await?,
    };
    Ok(Json(json!({ "tab": tab.describe().await, "created": created, "observation": observation })))
}

async fn close_tab(State(b): State<AppState>, Path(tid): Path<String>) -> Result<Json<Value>> {
    b.close_tab(&tid).await?;
    Ok(Json(json!({ "closed": tid })))
}

#[derive(Deserialize, Default)]
struct ObserveReq {
    text_chars: Option<u64>,
    max_candidates: Option<u64>,
}

async fn observe(State(b): State<AppState>, Path(tid): Path<String>, body: Option<Json<ObserveReq>>) -> Result<Json<Value>> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    Ok(Json(b.tab(&tid).await?.observe(req.text_chars, req.max_candidates).await?))
}

#[derive(Deserialize)]
struct ActReq {
    observation_id: u64,
    action_id: String,
    text: Option<String>,
}

async fn act(State(b): State<AppState>, Path(tid): Path<String>, Json(req): Json<ActReq>) -> Result<Json<Value>> {
    Ok(Json(b.tab(&tid).await?.act(req.observation_id, &req.action_id, req.text.as_deref()).await?))
}

#[derive(Deserialize)]
struct NavigateReq {
    url: String,
}

async fn navigate(State(b): State<AppState>, Path(tid): Path<String>, Json(req): Json<NavigateReq>) -> Result<Json<Value>> {
    Ok(Json(b.tab(&tid).await?.navigate(&req.url).await?))
}

#[derive(Deserialize, Default)]
struct ReadReq {
    max_chars: Option<u64>,
}

async fn read(State(b): State<AppState>, Path(tid): Path<String>, body: Option<Json<ReadReq>>) -> Result<Json<Value>> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    Ok(Json(b.tab(&tid).await?.read(req.max_chars).await?))
}

async fn screenshot(State(b): State<AppState>, Path(tid): Path<String>) -> Result<Json<Value>> {
    Ok(Json(b.tab(&tid).await?.screenshot().await?))
}

async fn handover_start(State(b): State<AppState>, Path(tid): Path<String>) -> Result<Json<Value>> {
    Ok(Json(b.handover_start(&tid).await?))
}

async fn handover_end(State(b): State<AppState>, Path(tid): Path<String>) -> Result<Json<Value>> {
    Ok(Json(b.handover_end(&tid).await?))
}

/// The daemon's pipe to the extension. Only the daemon can reach this route
/// (loopback + the per-launch runtime token); it has already verified the
/// extension's Origin and pairing token.
async fn extension_driver(State(b): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| relay_session(b, socket)).into_response()
}

async fn relay_session(browser: Arc<Browser>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let relay = RelayConn::new(tx);
    browser.set_relay(relay.clone()).await;
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(Message::Text(frame)).await.is_err() {
                break;
            }
        }
    });
    let mut verified = false;
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                relay.handle_frame(&text).await;
                if verified {
                    continue;
                }
                if let Some(hello) = relay.hello().await {
                    let got = hello.get("scripts").and_then(Value::as_str).unwrap_or_default().to_string();
                    if got != scripts::bundle_hash() {
                        // An extension built for another script bundle would refuse
                        // (or worse, accept) scripts this runtime does not send.
                        tracing::warn!("extension built for script bundle {got}, runtime has {}", scripts::bundle_hash());
                        relay.notify("reject", json!({ "code": "scripts_mismatch", "expected": scripts::bundle_hash(), "got": got }));
                        break;
                    }
                    verified = true;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    browser.drop_relay(&relay).await;
    // Let a last frame (a rejection) reach the extension before closing.
    let abort = writer.abort_handle();
    if tokio::time::timeout(std::time::Duration::from_millis(200), writer).await.is_err() {
        abort.abort();
    }
}

