//! HTTP input: the long-running server that exposes the registry over HTTP and
//! performs path-based reverse proxying. It is a thin transport layer over
//! [`Daemon`]; all registry mutations go through [`Command`].

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::daemon::{Daemon, Service};

#[derive(Deserialize)]
struct RegisterReq {
    name: String,
    url: String,
    #[serde(default)]
    description: String,
}

#[derive(Serialize)]
struct RegisterRes {
    success: bool,
    id: u64,
}

#[derive(Deserialize)]
struct UnregisterReq {
    id: u64,
}

#[derive(Serialize)]
struct UnregisterRes {
    success: bool,
}

/// Public-facing service info (used by `/{name}/list`).
#[derive(Serialize)]
struct ServiceInfo {
    id: u64,
    name: String,
    url: String,
    description: String,
}

impl ServiceInfo {
    fn from(id: u64, s: &Service) -> Self {
        ServiceInfo {
            id,
            name: s.name.clone(),
            url: s.url.clone(),
            description: s.description.clone(),
        }
    }
}

/// GET /list — list all registered services as { name: description }.
async fn list_all(State(daemon): State<Daemon>) -> Json<HashMap<String, String>> {
    Json(daemon.list())
}

/// GET /{name}/list — list all instances under the given name.
async fn list_by_name(
    State(daemon): State<Daemon>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<Vec<ServiceInfo>> {
    let list = daemon
        .list_by_name(&name)
        .iter()
        .map(|(id, s)| ServiceInfo::from(*id, s))
        .collect();
    Json(list)
}

/// POST /registry — a service registers itself.
async fn register(State(daemon): State<Daemon>, Json(req): Json<RegisterReq>) -> Json<RegisterRes> {
    let id = daemon.register(req.name, req.url, req.description);
    Json(RegisterRes { success: true, id })
}

/// POST /unregistry — a service gracefully goes offline.
async fn unregister(
    State(daemon): State<Daemon>,
    Json(req): Json<UnregisterReq>,
) -> Json<UnregisterRes> {
    let success = daemon.unregister(req.id);
    Json(UnregisterRes { success })
}

/// Path-based forwarding: GET/POST/... /{name}/{id}/...
/// Strips the /{name}/{id} prefix and forwards to the backend base address.
async fn proxy(State(daemon): State<Daemon>, req: Request) -> Response {
    // Parse name and id from the path manually (avoids the Path extractor's
    // argument-count mismatch in wildcard/trailing-slash cases).
    let path = req.uri().path().to_string();
    let mut segs = path.splitn(4, '/').skip(1); // drop the leading empty segment
    let name = segs.next().unwrap_or("").to_string();
    let id: u64 = match segs.next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return (StatusCode::NOT_FOUND, "invalid path").into_response(),
    };

    // Look up the target service.
    let service = match daemon.get(id) {
        Some(s) if s.name == name => s,
        Some(_) => return (StatusCode::NOT_FOUND, "service name/id mismatch").into_response(),
        None => return (StatusCode::NOT_FOUND, "service not found").into_response(),
    };

    // Compute the remaining path after stripping the prefix.
    // Only accept the trailing-slash form /{name}/{id}/...; /{name}/{id} is an error.
    let prefix = format!("/{name}/{id}");
    let rest = match path.strip_prefix(&prefix) {
        Some(r) if r.starts_with('/') => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                format!("invalid path: use {prefix}/ (trailing slash required)"),
            )
                .into_response();
        }
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!("{}{}{}", service.url, rest, query);

    // Rebuild the request and send it to the backend.
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid request body").into_response(),
    };

    let upstream = daemon
        .client
        .request(method, &target)
        .headers(headers)
        .body(body)
        .send()
        .await;

    match upstream {
        Ok(resp) => {
            let status = resp.status();
            let resp_headers = resp.headers().clone();
            let stream = resp.bytes_stream();
            let mut builder = Response::builder().status(status);
            if let Some(h) = builder.headers_mut() {
                *h = resp_headers;
            }
            builder
                .body(Body::from_stream(stream))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(err) => {
            // Backend unreachable -> active deregistration.
            tracing::warn!(%target, error = %err, "forwarding failed");
            daemon.deregister(id);
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

/// Build the axum router wired to the daemon.
fn router(daemon: Daemon) -> Router {
    Router::new()
        .route("/registry", post(register))
        .route("/unregistry", post(unregister))
        .route("/list", get(list_all))
        .route("/{name}/list", get(list_by_name))
        // All other paths go to the dynamic forwarding handler.
        .fallback(proxy)
        .with_state(daemon)
}

/// Best-effort discovery of the primary LAN IP. Opens a UDP socket and
/// "connects" it to a public address; no packets are sent, but the OS picks
/// the outbound interface, whose local address is the LAN IP.
fn local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// Print the reachable API endpoints on startup.
fn print_endpoints(bound: std::net::SocketAddr) {
    let port = bound.port();
    println!("switchelo is running:");
    if bound.ip().is_unspecified() {
        // Bound to 0.0.0.0 / [::] -> reachable on loopback and the LAN.
        println!("  http://localhost:{port}/");
        if let Some(ip) = local_ip() {
            println!("  http://{ip}:{port}/");
        }
    } else {
        println!("  http://{bound}/");
    }
}

/// Run the HTTP input: bind `addr` and serve until the process exits.
pub async fn serve(daemon: Daemon, addr: &str) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "error: address {addr} is already in use \
                 (another switchelo instance? set BIND to a free port)"
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    if let Ok(bound) = listener.local_addr() {
        print_endpoints(bound);
    }
    tracing::info!("switchelo listening on {addr}");
    if let Err(e) = axum::serve(listener, router(daemon)).await {
        eprintln!("error: server stopped: {e}");
        std::process::exit(1);
    }
}
