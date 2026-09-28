//! `sen-browser` — SenClaw's browser runtime.
//!
//! The runtime is the *hands and eyes* of the browser agent: it observes a page
//! in one atomic call, executes one guarded action with trusted CDP input, and
//! hands a tab to the person when a login is needed. It never sees the goal and
//! never calls a model — the decision loop lives in the daemon's control plane.
//!
//! Two drivers share one core:
//! - [`chrome`] launches and owns a Chrome with its own profile (managed);
//! - [`relay`] drives tabs in the user's own Chrome through the SenClaw
//!   extension's `chrome.debugger`, relayed by the daemon.
//!
//! Everything above [`cdp::TabChannel`] — snapshot, guards, execution, settle —
//! is written once in [`tab`].

pub mod allowlist;
pub mod browser;
pub mod cdp;
pub mod chrome;
pub mod error;
pub mod http;
pub mod observation;
pub mod relay;
pub mod scripts;
pub mod tab;

pub use browser::Browser;
