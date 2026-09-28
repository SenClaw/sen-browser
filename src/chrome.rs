//! The managed driver: a Chrome this runtime launches with its own profile.
//!
//! Chrome 136+ refuses remote debugging on the default profile, and the agent
//! must never share a profile with the person anyway, so every profile lives
//! under the runtime's data dir (`profiles/<name>`, mode 0700). Chrome picks a
//! random loopback debugging port and writes it to `DevToolsActivePort` inside
//! that profile; nothing else needs to know it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::cdp::WsConnection;
use crate::error::{BrowserError, Result};

const LAUNCH_DEADLINE: Duration = Duration::from_secs(20);

/// Find a Chromium-family browser. `SEN_BROWSER_CHROME` wins.
pub fn find_chrome() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SEN_BROWSER_CHROME") {
        let p = PathBuf::from(p.trim());
        if p.is_file() {
            return Ok(p);
        }
        return Err(BrowserError::BrowserNotFound(format!("SEN_BROWSER_CHROME points at {} which does not exist", p.display())));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates: Vec<String> = if cfg!(target_os = "macos") {
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
        ];
        apps.iter()
            .flat_map(|a| [format!("/Applications/{a}"), format!("{home}/Applications/{a}")])
            .collect()
    } else if cfg!(target_os = "windows") {
        let roots = [
            std::env::var("PROGRAMFILES").unwrap_or_else(|_| "C:\\Program Files".into()),
            std::env::var("PROGRAMFILES(X86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into()),
            std::env::var("LOCALAPPDATA").unwrap_or_default(),
        ];
        roots
            .iter()
            .flat_map(|r| {
                [
                    format!("{r}\\Google\\Chrome\\Application\\chrome.exe"),
                    format!("{r}\\Microsoft\\Edge\\Application\\msedge.exe"),
                    format!("{r}\\Chromium\\Application\\chrome.exe"),
                ]
            })
            .collect()
    } else {
        ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge"]
            .iter()
            .flat_map(|b| ["/usr/bin", "/usr/local/bin", "/snap/bin"].map(|d| format!("{d}/{b}")))
            .collect()
    };
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| BrowserError::BrowserNotFound("install Google Chrome, or set SEN_BROWSER_CHROME".into()))
}

/// Profile names become directory names.
pub fn profile_dir(data_dir: &Path, profile: &str) -> Result<PathBuf> {
    let ok = !profile.is_empty()
        && profile.len() <= 40
        && profile.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !ok {
        return Err(BrowserError::BadRequest(format!("profile name `{profile}` must be 1-40 of [a-z0-9_-]")));
    }
    Ok(data_dir.join("profiles").join(profile))
}

/// On macOS Chrome encrypts cookies with a key from the login keychain and
/// blocks its first network request until it gets one. A HOME without a
/// keychain (a scratch or service account) would therefore hang every page
/// load, so Chrome gets its built-in mock keychain there. A real account keeps
/// the keychain: the managed profile's sign-ins stay encrypted with it.
fn needs_mock_keychain(home: Option<&Path>) -> bool {
    cfg!(target_os = "macos") && !home.map(|h| h.join("Library/Keychains").is_dir()).unwrap_or(false)
}

pub struct ManagedChrome {
    pub conn: Arc<WsConnection>,
    pub profile_dir: PathBuf,
    pub headless: bool,
    child: Mutex<Option<Child>>,
    pids_file: PathBuf,
}

impl ManagedChrome {
    pub async fn launch(binary: &Path, profile_dir: &Path, headless: bool, pids_file: &Path) -> Result<ManagedChrome> {
        std::fs::create_dir_all(profile_dir).map_err(|e| BrowserError::Internal(format!("cannot create profile dir: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(profile_dir, std::fs::Permissions::from_mode(0o700));
        }
        let port_file = profile_dir.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);

        let mut cmd = Command::new(binary);
        cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg("--remote-debugging-port=0")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-timer-throttling")
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--disable-renderer-backgrounding")
            .arg("--window-size=1120,860");
        if headless {
            cmd.arg("--headless=new");
        }
        if needs_mock_keychain(std::env::var_os("HOME").map(PathBuf::from).as_deref()) {
            cmd.arg("--use-mock-keychain");
        }
        cmd.arg("about:blank").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| BrowserError::BrowserNotFound(format!("could not start {}: {e}", binary.display())))?;
        if let Some(pid) = child.id() {
            remember_pid(pids_file, pid, profile_dir);
        }

        let started = Instant::now();
        let ws_url = loop {
            if let Ok(text) = std::fs::read_to_string(&port_file) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    if !port.trim().is_empty() && path.starts_with('/') {
                        break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                    }
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(BrowserError::Internal(format!(
                    "Chrome exited ({status}) before it was ready; is another Chrome already using {}?",
                    profile_dir.display()
                )));
            }
            if started.elapsed() > LAUNCH_DEADLINE {
                let _ = child.kill().await;
                return Err(BrowserError::Internal("Chrome did not open its debugging port within 20 s".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let conn = WsConnection::connect(&ws_url).await?;
        Ok(ManagedChrome {
            conn,
            profile_dir: profile_dir.to_path_buf(),
            headless,
            child: Mutex::new(Some(child)),
            pids_file: pids_file.to_path_buf(),
        })
    }

    /// A new background tab, attached as a flat session.
    pub async fn open_target(&self, url: &str) -> Result<(String, String)> {
        let created = self.conn.call("Target.createTarget", json!({ "url": url, "background": true }), None).await?;
        let target = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Internal("Target.createTarget returned no targetId".into()))?
            .to_string();
        let attached = self.conn.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None).await?;
        let session = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Internal("Target.attachToTarget returned no sessionId".into()))?
            .to_string();
        Ok((target, session))
    }

    pub async fn close_target(&self, target: &str) {
        let _ = self.conn.call("Target.closeTarget", json!({ "targetId": target }), None).await;
    }

    pub async fn version(&self) -> Value {
        self.conn.call("Browser.getVersion", json!({}), None).await.unwrap_or(Value::Null)
    }

    /// Close Chrome: politely first, then by force.
    pub async fn shutdown(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.conn.call("Browser.close", json!({}), None)).await;
        if let Some(mut child) = self.child.lock().await.take() {
            // `Child::id()` is `None` once the process has been reaped.
            let pid = child.id();
            if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
            if let Some(pid) = pid {
                forget_pid(&self.pids_file, pid);
            }
        }
        let _ = std::fs::remove_file(self.profile_dir.join("DevToolsActivePort"));
    }
}

fn read_pids(file: &Path) -> Vec<(u32, String)> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<(u32, String)>>(&t).ok())
        .unwrap_or_default()
}

fn remember_pid(file: &Path, pid: u32, profile_dir: &Path) {
    let mut pids = read_pids(file);
    pids.push((pid, profile_dir.display().to_string()));
    let _ = std::fs::write(file, serde_json::to_string(&pids).unwrap_or_default());
}

fn forget_pid(file: &Path, pid: u32) {
    let pids: Vec<_> = read_pids(file).into_iter().filter(|(p, _)| *p != pid).collect();
    let _ = std::fs::write(file, serde_json::to_string(&pids).unwrap_or_default());
}

/// Stop Chrome processes a previous run of this runtime left behind (the
/// runtime can be killed without running destructors). A pid is stopped only
/// when its command line still names the profile we launched it with — a
/// recycled pid belongs to someone else and is left alone.
pub fn reap_orphans(file: &Path) {
    for (pid, profile) in read_pids(file) {
        #[cfg(unix)]
        {
            let out = std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output();
            if let Ok(out) = out {
                let command = String::from_utf8_lossy(&out.stdout);
                if command.contains(&format!("--user-data-dir={profile}")) {
                    // SAFETY: plain kill(2) on a pid we verified is our Chrome.
                    unsafe {
                        libc_kill(pid as i32);
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (pid, profile);
        }
    }
    let _ = std::fs::remove_file(file);
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, 15);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_are_directory_safe() {
        let base = Path::new("/tmp/x");
        assert!(profile_dir(base, "default").is_ok());
        assert!(profile_dir(base, "work_2").is_ok());
        assert!(profile_dir(base, "../etc").is_err());
        assert!(profile_dir(base, "").is_err());
        assert!(profile_dir(base, "Upper").is_err());
    }

    #[test]
    fn mock_keychain_only_without_a_login_keychain() {
        let bare = tempfile::tempdir().unwrap();
        let account = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(account.path().join("Library/Keychains")).unwrap();
        assert!(!needs_mock_keychain(Some(account.path())), "a real account keeps its keychain");
        assert_eq!(needs_mock_keychain(Some(bare.path())), cfg!(target_os = "macos"));
        assert_eq!(needs_mock_keychain(None), cfg!(target_os = "macos"));
    }
}
