//! The runtime's error vocabulary. Every variant maps to one HTTP status and a
//! machine-readable `code`, in the `{"error", "code"}` shape the daemon reads
//! from every runtime (`sen_runtime_sdk::api`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// The page changed since the observation the action was chosen from.
    /// Nothing was sent to the page, so observing again is always safe.
    #[error("the page changed since this observation; observe again")]
    StalePage,
    /// Hit-testing found another element on top of the target. No input sent.
    #[error("the target is covered by another element{0}")]
    TargetCovered(String),
    /// The target no longer exists, is hidden, disabled, read-only or off screen.
    #[error("the target cannot receive this action: {0}")]
    InvalidTarget(String),
    /// The action may already have changed the page (for example a `<select>`
    /// whose change event fired before navigation destroyed the evaluation).
    /// Never retried automatically.
    #[error("the action was interrupted after it may have changed the page: {0}")]
    UncertainMutation(String),
    #[error("a JavaScript dialog is open; accept or dismiss it first")]
    DialogOpen,
    #[error("the tab is handed over to a person; wait until they finish")]
    HandoverActive,
    /// The person clicked into the agent's tab; the agent yields until resumed.
    #[error("the person is using this tab; the agent paused until they resume it")]
    UserActive,
    /// The person cancelled the debugger session from Chrome's bar. Never
    /// re-attached automatically.
    #[error("the person cancelled debugging of this tab; resume to attach again")]
    Detached,
    #[error("the SenClaw extension is not connected")]
    ExtensionNotConnected,
    #[error("no browser was found: {0}")]
    BrowserNotFound(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("{0}")]
    Internal(String),
}

impl BrowserError {
    pub fn code(&self) -> &'static str {
        match self {
            BrowserError::StalePage => "stale_page",
            BrowserError::TargetCovered(_) => "target_covered",
            BrowserError::InvalidTarget(_) => "invalid_target",
            BrowserError::UncertainMutation(_) => "uncertain_mutation",
            BrowserError::DialogOpen => "dialog_open",
            BrowserError::HandoverActive => "handover_active",
            BrowserError::UserActive => "user_active",
            BrowserError::Detached => "debugger_detached",
            BrowserError::ExtensionNotConnected => "extension_not_connected",
            BrowserError::BrowserNotFound(_) => "browser_not_found",
            BrowserError::NotFound(_) => "not_found",
            BrowserError::BadRequest(_) => "bad_request",
            BrowserError::Blocked(_) => "blocked",
            BrowserError::Internal(_) => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            BrowserError::StalePage | BrowserError::TargetCovered(_) => StatusCode::CONFLICT,
            BrowserError::InvalidTarget(_) | BrowserError::BadRequest(_) => StatusCode::UNPROCESSABLE_ENTITY,
            BrowserError::DialogOpen | BrowserError::HandoverActive | BrowserError::UserActive | BrowserError::Detached => {
                StatusCode::LOCKED
            }
            BrowserError::ExtensionNotConnected | BrowserError::BrowserNotFound(_) => StatusCode::SERVICE_UNAVAILABLE,
            BrowserError::NotFound(_) => StatusCode::NOT_FOUND,
            BrowserError::Blocked(_) => StatusCode::FORBIDDEN,
            BrowserError::UncertainMutation(_) | BrowserError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<crate::cdp::CdpError> for BrowserError {
    fn from(e: crate::cdp::CdpError) -> Self {
        match e {
            crate::cdp::CdpError::Closed => BrowserError::Internal("the browser connection closed".into()),
            crate::cdp::CdpError::Blocked(m) => BrowserError::Blocked(m),
            other => BrowserError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for BrowserError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.to_string(), "code": self.code() });
        (self.status(), Json(body)).into_response()
    }
}

pub type Result<T> = std::result::Result<T, BrowserError>;
