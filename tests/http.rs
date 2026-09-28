//! The real binary under the SDK scaffold: health, bearer auth, info, status.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn serves_health_auth_info_status_and_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_sen-browser"))
        .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
        .env("SENCLAW_RUNTIME_TOKEN", "test-token")
        .env("SENCLAW_RUNTIME_DATA_DIR", dir.path())
        .env_remove("SENCLAW_PARENT_PID")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();

    let mut healthy = false;
    for _ in 0..100 {
        if let Ok(r) = http.get(format!("{base}/health")).send().await {
            if r.status().is_success() {
                let body: Value = r.json().await.unwrap();
                assert_eq!(body["status"], "ok");
                assert_eq!(body["id"], "sen-browser");
                healthy = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(healthy, "the runtime answered /health");

    // Every route but /health needs the per-launch token.
    assert_eq!(http.get(format!("{base}/v1/status")).send().await.unwrap().status(), 401);
    let status: Value = http.get(format!("{base}/v1/status")).bearer_auth("test-token").send().await.unwrap().json().await.unwrap();
    assert_eq!(status["extension"]["connected"], false);
    assert!(status["scripts"].as_str().unwrap().starts_with("sha256:"));

    let info: Value = http.get(format!("{base}/runtime/info")).bearer_auth("test-token").send().await.unwrap().json().await.unwrap();
    assert_eq!(info["id"], "sen-browser");
    assert_eq!(info["detail"]["scripts"], status["scripts"]);

    let scripts: Value = http.get(format!("{base}/v1/scripts")).bearer_auth("test-token").send().await.unwrap().json().await.unwrap();
    assert_eq!(scripts["bundle"], status["scripts"]);

    // An extension session without an extension is a clear 503.
    let r = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth("test-token")
        .json(&serde_json::json!({ "driver": "extension" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["code"], "extension_not_connected");

    let r = http.post(format!("{base}/runtime/shutdown")).bearer_auth("test-token").send().await.unwrap();
    assert!(r.status().is_success());
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
