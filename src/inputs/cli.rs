//! Command-line front-end.
//!
//! `switchelo` with no subcommand runs the daemon (see [`crate::inputs::http`]).
//! `switchelo register|unregister ...` acts as a *client*: it makes sure a
//! daemon is running — auto-starting one in the background if not — and then
//! sends the request to it over HTTP, reusing the `reqwest` client the proxy
//! already depends on.

use std::process::Stdio;
use std::time::Duration;

/// What the invocation asked switchelo to do.
pub enum Action {
    /// Run the daemon (HTTP server) in the foreground.
    Serve { bind: String },
    /// Register (or refresh) a service against a running daemon.
    Register {
        bind: String,
        name: String,
        url: String,
        description: String,
    },
    /// Deregister a service by id against a running daemon.
    Unregister { bind: String, id: u64 },
}

const HELP: &str = "\
switchelo — a dynamic service registry and reverse proxy

USAGE:
    switchelo [--bind <ADDR>]                       Run the daemon
    switchelo register <NAME> <URL> [DESCRIPTION]   Register a service
    switchelo unregister <ID>                       Deregister a service

The register/unregister subcommands auto-start the daemon in the background if
one is not already running, then send the request over HTTP.

OPTIONS:
    -b, --bind <ADDR>   Daemon listen/connect address (default: 0.0.0.0:8080, or $BIND)
    -h, --help          Print this help and exit

ENVIRONMENT:
    BIND        Default address.
    RUST_LOG    Log filter (default: switchelo=info).

EXAMPLES:
    switchelo
    switchelo register api http://127.0.0.1:8081 \"main api\"
    switchelo unregister 0";

impl Action {
    /// The bind/connect address carried by every action.
    fn bind(&self) -> &str {
        match self {
            Action::Serve { bind }
            | Action::Register { bind, .. }
            | Action::Unregister { bind, .. } => bind,
        }
    }

    /// Parse process arguments. Prints help / errors and exits on misuse.
    pub fn parse() -> Action {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let mut positional: Vec<String> = Vec::new();

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-h" | "--help" => {
                    println!("{HELP}");
                    std::process::exit(0);
                }
                "-b" | "--bind" => {
                    i += 1;
                    bind = args
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| fail("--bind requires an address"));
                }
                s if s.starts_with('-') => fail(&format!("unknown option: {s}\n\n{HELP}")),
                _ => positional.push(args[i].clone()),
            }
            i += 1;
        }

        match positional.first().map(String::as_str) {
            None => Action::Serve { bind },
            Some("register") => {
                let name = positional
                    .get(1)
                    .cloned()
                    .unwrap_or_else(|| fail("register requires <NAME> <URL> [DESCRIPTION]"));
                let url = positional
                    .get(2)
                    .cloned()
                    .unwrap_or_else(|| fail("register requires <NAME> <URL> [DESCRIPTION]"));
                let description = positional.get(3).cloned().unwrap_or_default();
                Action::Register {
                    bind,
                    name,
                    url,
                    description,
                }
            }
            Some("unregister") => {
                let id = positional
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| fail("unregister requires a numeric <ID>"));
                Action::Unregister { bind, id }
            }
            Some(other) => fail(&format!("unknown subcommand: {other}\n\n{HELP}")),
        }
    }
}

/// Run a client action: ensure the daemon is up, then send the request.
pub async fn run_client(action: Action) {
    let bind = action.bind().to_string();
    let base = client_base_url(&bind);
    ensure_daemon(&base, &bind).await;

    let client = http_client(Duration::from_secs(5));
    match action {
        Action::Register {
            name,
            url,
            description,
            ..
        } => {
            let body = serde_json::json!({
                "name": name, "url": url, "description": description,
            });
            let text = post(&client, &format!("{base}/registry"), &body).await;
            let id = parse_field(&text, "id");
            println!("registered '{name}' -> {url} (id={id}); route: /{name}/{id}/");
        }
        Action::Unregister { id, .. } => {
            let body = serde_json::json!({ "id": id });
            let text = post(&client, &format!("{base}/unregistry"), &body).await;
            match parse_field(&text, "success").as_str() {
                "true" => println!("unregistered id={id}"),
                _ => println!("no service with id={id} was registered"),
            }
        }
        Action::Serve { .. } => unreachable!("serve is handled by the daemon path"),
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

async fn post(client: &reqwest::Client, url: &str, body: &serde_json::Value) -> String {
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap_or_else(|e| fail(&format!("request to {url} failed: {e}")));
    resp.text()
        .await
        .unwrap_or_else(|e| fail(&format!("reading response from {url} failed: {e}")))
}

/// Extract a top-level field from a JSON object response as a string.
fn parse_field(text: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get(key).map(|f| f.to_string()))
        .unwrap_or_else(|| fail(&format!("unexpected daemon response: {text}")))
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
