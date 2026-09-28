//! The only JavaScript the runtime ever runs inside a page.
//!
//! Each script is a function declaration sent with `Runtime.callFunctionOn`
//! into SenClaw's isolated world; its arguments travel as data, never as code.
//! The SHA-256 of every declaration is the allowlist the extension enforces on
//! the user's own Chrome, so even a compromised daemon or runtime cannot run
//! arbitrary JavaScript on a logged-in page. [`bundle_hash`] versions the set:
//! the extension refuses a runtime whose bundle it was not built with.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub struct Script {
    pub name: &'static str,
    source: &'static str,
}

impl Script {
    /// Exactly what is sent as `functionDeclaration` — and hashed.
    pub fn declaration(&self) -> &'static str {
        self.source.trim()
    }

    pub fn sha256(&self) -> String {
        sha256_hex(self.declaration())
    }
}

pub const SNAPSHOT: Script = Script { name: "snapshot", source: include_str!("../scripts/snapshot.js") };
pub const GUARD: Script = Script { name: "guard", source: include_str!("../scripts/guard.js") };
pub const ACT: Script = Script { name: "act", source: include_str!("../scripts/act.js") };
pub const SETTLE: Script = Script { name: "settle", source: include_str!("../scripts/settle.js") };
pub const READ: Script = Script { name: "read", source: include_str!("../scripts/read.js") };

pub const ALL: [&Script; 5] = [&SNAPSHOT, &GUARD, &ACT, &SETTLE, &READ];

pub fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Whether `declaration` is one of the approved scripts, byte for byte.
pub fn is_allowed(declaration: &str) -> bool {
    let digest = sha256_hex(declaration);
    ALL.iter().any(|s| s.sha256() == digest)
}

/// One hash for the whole set, in a fixed order.
pub fn bundle_hash() -> String {
    let lines: String = ALL.iter().map(|s| format!("{}:{}\n", s.name, s.sha256())).collect();
    format!("sha256:{}", sha256_hex(&lines))
}

/// `{bundle, scripts: [{name, sha256}]}` — what the extension build embeds.
pub fn manifest() -> Value {
    json!({
        "bundle": bundle_hash(),
        "scripts": ALL.iter().map(|s| json!({ "name": s.name, "sha256": s.sha256() })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_script_is_a_function_declaration() {
        for s in ALL {
            assert!(s.declaration().starts_with("function ("), "{} must be a function declaration", s.name);
            assert!(s.declaration().ends_with('}'), "{} must end with its closing brace", s.name);
        }
    }

    #[test]
    fn allowlist_accepts_exact_scripts_only() {
        assert!(is_allowed(SNAPSHOT.declaration()));
        assert!(!is_allowed(&format!("{} ", SNAPSHOT.declaration())));
        assert!(!is_allowed("function () { return document.cookie; }"));
    }

    #[test]
    fn bundle_hash_is_stable_and_prefixed() {
        assert_eq!(bundle_hash(), bundle_hash());
        assert!(bundle_hash().starts_with("sha256:"));
        assert_eq!(manifest()["scripts"].as_array().unwrap().len(), ALL.len());
    }
}
