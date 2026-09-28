//! `sen-browser serve --host H --port P` — launched by the SenClaw daemon.
//! `sen-browser scripts` — print the approved script bundle (for the extension build).

use std::sync::Arc;

use sen_runtime_sdk::env::LaunchEnv;
use sen_runtime_sdk::manifest::RunMode;
use sen_runtime_sdk::server::{self, Readiness, ServeArgs, ServeOptions};
use sen_browser::{http, scripts, Browser};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {
            let rest: Vec<String> = args.collect();
            tokio::runtime::Runtime::new()?.block_on(serve(rest))
        }
        Some("scripts") => {
            println!("{}", serde_json::to_string_pretty(&scripts::manifest())?);
            Ok(())
        }
        Some("--version") | Some("version") => {
            println!("sen-browser {VERSION}");
            Ok(())
        }
        _ => {
            eprintln!("usage: sen-browser serve --host 127.0.0.1 --port 4970\n       sen-browser scripts");
            std::process::exit(2);
        }
    }
}

async fn serve(rest: Vec<String>) -> anyhow::Result<()> {
    server::init_tracing();
    let env = LaunchEnv::from_env("sen-browser", VERSION);
    let args = ServeArgs::parse(rest).map_err(|e| anyhow::anyhow!(e))?;
    let browser = Browser::new(env.data_dir.clone());
    let routes = http::router(browser.clone());
    let bundle = scripts::bundle_hash();
    let info: Arc<dyn Fn() -> serde_json::Value + Send + Sync> = Arc::new(move || {
        serde_json::json!({
            "scripts": bundle,
            "chrome": sen_browser::chrome::find_chrome().map(|p| p.display().to_string()).ok(),
        })
    });
    let result = server::serve(
        routes,
        ServeOptions {
            env,
            mode: RunMode::Service,
            // The SDK's capability enum learns `browser` with the daemon's
            // browser slot; until then /runtime/info lists none.
            capabilities: vec![],
            readiness: Readiness::ready(),
            info_detail: Some(info),
            args,
        },
    )
    .await;
    browser.shutdown().await;
    result
}
