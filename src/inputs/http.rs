//! HTTP input: the long-running server that exposes the registry over HTTP and
//! performs path-based reverse proxying. It is a thin transport layer over
//! [`Daemon`]; all registry mutations go through the daemon's methods. The JSON
//! shapes live in [`super::wire`], shared with the CLI client.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use std::collections::HashMap;

use crate::daemon::Daemon;
use crate::inputs::wire::{
    Instance, RegisterReq, RegisterRes, ServiceGroup, UnregisterReq, UnregisterRes,
};

/// GET /list — every service grouped by name, each with its instances.
async fn list(State(daemon): State<Daemon>) -> Json<HashMap<String, ServiceGroup>> {
    let mut grouped: HashMap<String, ServiceGroup> = HashMap::new();
    for (id, s) in daemon.snapshot() {
        let group = grouped.entry(s.name).or_insert_with(|| ServiceGroup {
            description: s.description,
            services: HashMap::new(),
        });
        group.services.insert(id, Instance { url: s.url });
    }
    Json(grouped)
}

/// POST /registry — a service registers itself.
async fn register(State(daemon): State<Daemon>, Json(req): Json<RegisterReq>) -> Json<RegisterRes> {
    let id = daemon.register(req.name, req.url, req.description);
    Json(RegisterRes { id })
}

/// POST /unregistry — a service gracefully goes offline.
async fn unregister(
    State(daemon): State<Daemon>,
    Json(req): Json<UnregisterReq>,
) -> Json<UnregisterRes> {
    let success = daemon.unregister(req.id);
    if success {
        tracing::info!(id = req.id, "service unregistered");
    }
    Json(UnregisterRes { success })
}

/// Proxy to the backend root: /{name}/{id}
async fn proxy_root(
    State(daemon): State<Daemon>,
    Path((name, id)): Path<(String, u64)>,
    req: Request,
) -> Response {
    forward(daemon, name, id, "", req).await
}

/// Proxy to a backend sub-path: /{name}/{id}/{*rest}
async fn proxy_path(
    State(daemon): State<Daemon>,
    Path((name, id, rest)): Path<(String, u64, String)>,
    req: Request,
) -> Response {
    forward(daemon, name, id, &rest, req).await
}

/// Forward `req` to backend `id` (verifying its name) at `/{rest}`. `rest` has
/// no leading slash; an empty `rest` targets the backend root.
async fn forward(daemon: Daemon, name: String, id: u64, rest: &str, req: Request) -> Response {
    let service = match daemon.get(id) {
        Some(s) if s.name == name => s,
        Some(_) => return (StatusCode::NOT_FOUND, "service name/id mismatch").into_response(),
        None => return (StatusCode::NOT_FOUND, "service not found").into_response(),
    };

    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!("{}/{}{}", service.url, rest, query);

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
            // Backend reachable — clear any failure streak.
            daemon.record_success(id);
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
            // Backend unreachable -> count it; only a sustained streak evicts.
            tracing::warn!(%target, error = %err, "forwarding failed");
            if daemon.record_failure(id) {
                tracing::warn!(id, "service deregistered after consecutive forwarding failures");
            }
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

/// Build the axum router wired to the daemon.
fn router(daemon: Daemon) -> Router {
    Router::new()
        .route("/registry", post(register))
        .route("/unregistry", post(unregister))
        .route("/list", get(list))
        // Path-based forwarding. axum can't bind an optional trailing capture in
        // one handler (the 2-arg route would fail Path's arity check), so the two
        // routes split to two handlers that share `forward`.
        .route("/{name}/{id}", any(proxy_root))
        .route("/{name}/{id}/{*rest}", any(proxy_path))
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
