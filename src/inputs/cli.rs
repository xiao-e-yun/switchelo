//! Command-line front-end.
//!
//! `switchelo` with no subcommand runs the daemon (see [`crate::inputs::http`]).
//! `switchelo register|unregister ...` acts as a *client*: it makes sure a
//! daemon is running — auto-starting one in the background if not — and then
//! sends the request to it over HTTP, reusing the `reqwest` client the proxy
//! already depends on.

use std::process::Stdio;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::inputs::wire::{RegisterReq, RegisterRes, UnregisterReq, UnregisterRes};

/// switchelo — a dynamic service registry and reverse proxy.
#[derive(Parser)]
#[command(name = "switchelo", version, about, long_about = None)]
pub struct Cli {
    /// Daemon listen/connect address. A wildcard host (0.0.0.0) is dialed as
    /// 127.0.0.1 by the client subcommands.
    #[arg(short, long, env = "BIND", default_value = "0.0.0.0:8080", global = true)]
    pub bind: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Client subcommands. Each auto-starts the daemon if one is not running, then
/// sends the request over HTTP.
#[derive(Subcommand)]
pub enum Command {
    /// Register (or refresh) a service.
    Register {
        name: String,
        url: String,
        description: Option<String>,
    },
    /// Deregister a service by id.
    Unregister { id: u64 },
}

/// Run a client subcommand: ensure the daemon is up, then send the request.
pub async fn run_client(bind: &str, command: Command) {
    let base = client_base_url(bind);
    ensure_daemon(&base, bind).await;

    let client = http_client(Duration::from_secs(5));
    match command {
        Command::Register {
            name,
            url,
            description,
        } => {
            let req = RegisterReq {
                name: name.clone(),
                url: url.clone(),
                description: description.unwrap_or_default(),
            };
            let res: RegisterRes = post(&client, &format!("{base}/registry"), &req).await;
            let id = res.id;
            println!("registered '{name}' -> {url} (id={id}); route: /{name}/{id}/");
        }
        Command::Unregister { id } => {
            let req = UnregisterReq { id };
            let res: UnregisterRes = post(&client, &format!("{base}/unregistry"), &req).await;
            if res.success {
                println!("unregistered id={id}");
            } else {
                println!("no service with id={id} was registered");
            }
        }
    }
}

/// Ensure a daemon is reachable at `base`, auto-starting one bound to `bind`.
async fn ensure_daemon(base: &str, bind: &str) {
    let probe = http_client(Duration::from_millis(300));
    if reachable(&probe, base).await {
        return;
    }

    eprintln!("daemon not running; starting it in the background...");
    spawn_daemon(bind);

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if reachable(&probe, base).await {
            eprintln!("daemon ready on {bind}");
            return;
        }
    }
    fail("daemon did not become ready in time");
}

async fn reachable(client: &reqwest::Client, base: &str) -> bool {
    client.get(format!("{base}/list")).send().await.is_ok()
}

/// Spawn `switchelo --bind <bind>` as a detached background daemon.
fn spawn_daemon(bind: &str) {
    let exe = std::env::current_exe()
        .unwrap_or_else(|e| fail(&format!("cannot locate switchelo executable: {e}")));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--bind")
        .arg(bind)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    if let Err(e) = cmd.spawn() {
        fail(&format!("failed to start daemon: {e}"));
    }
}

/// Detach the child so it outlives this CLI process.
#[cfg(windows)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn detach(_cmd: &mut std::process::Command) {
    // On Unix, a spawned child with null stdio already survives the parent's
    // exit for our purposes; nothing extra is required.
}

/// POST `body` as JSON to `url` and deserialize the response into `R`.
async fn post<B: Serialize, R: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    body: &B,
) -> R {
    let json = serde_json::to_string(body).unwrap_or_else(|e| fail(&format!("encoding body: {e}")));
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(json)
        .send()
        .await
        .unwrap_or_else(|e| fail(&format!("request to {url} failed: {e}")));
    let text = resp
        .text()
        .await
        .unwrap_or_else(|e| fail(&format!("reading response from {url} failed: {e}")));
    serde_json::from_str(&text)
        .unwrap_or_else(|_| fail(&format!("unexpected daemon response: {text}")))
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Map a bind address to a client-connectable base URL. A wildcard host
/// (`0.0.0.0` / `[::]`) is replaced with loopback.
fn client_base_url(bind: &str) -> String {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "8080"));
    let host = match host {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}")
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
