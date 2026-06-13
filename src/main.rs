use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// A registered backend service.
#[derive(Clone, Debug)]
struct Service {
    name: String,
    /// Backend base address, e.g. http://127.0.0.1:8081
    url: String,
    description: String,
}

/// Shared state: registry + auto-increment id generator + HTTP client.
#[derive(Clone)]
struct AppState {
    services: Arc<RwLock<HashMap<u64, Service>>>,
    next_id: Arc<AtomicU64>,
    client: reqwest::Client,
}

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

/// Public-facing service info (used by /list and /{name}/list).
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

/// GET /list — list all registered services.
async fn list_all(State(state): State<AppState>) -> Json<Vec<ServiceInfo>> {
    let services = state.services.read().unwrap();
    let mut list: Vec<ServiceInfo> = services
        .iter()
        .map(|(id, s)| ServiceInfo::from(*id, s))
        .collect();
    list.sort_by_key(|i| i.id);
    Json(list)
}

/// GET /{name}/list — list all instances under the given name.
async fn list_by_name(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<Vec<ServiceInfo>> {
    let services = state.services.read().unwrap();
    let mut list: Vec<ServiceInfo> = services
        .iter()
        .filter(|(_, s)| s.name == name)
        .map(|(id, s)| ServiceInfo::from(*id, s))
        .collect();
    list.sort_by_key(|i| i.id);
    Json(list)
}

/// POST /registry — a service registers itself.
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Json<RegisterRes> {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let service = Service {
        name: req.name.clone(),
        // Trim trailing slash to avoid double slashes when forwarding.
        url: req.url.trim_end_matches('/').to_string(),
        description: req.description,
    };
    state.services.write().unwrap().insert(id, service);
    tracing::info!(id, name = %req.name, url = %req.url, "service registered");
    Json(RegisterRes { success: true, id })
}

/// POST /unregistry — a service gracefully goes offline.
async fn unregister(
    State(state): State<AppState>,
    Json(req): Json<UnregisterReq>,
) -> Json<UnregisterRes> {
    let removed = state.services.write().unwrap().remove(&req.id).is_some();
    if removed {
        tracing::info!(id = req.id, "service unregistered");
    }
    Json(UnregisterRes { success: removed })
}

/// Active deregistration: remove a service automatically when forwarding fails.
fn deregister(state: &AppState, id: u64) {
    if state.services.write().unwrap().remove(&id).is_some() {
        tracing::warn!(id, "service deregistered due to forwarding failure");
    }
}

/// Path-based forwarding: GET/POST/... /{name}/{id}/...
/// Strips the /{name}/{id} prefix and forwards to the backend base address.
async fn proxy(State(state): State<AppState>, req: Request) -> Response {
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
    let service = {
        let services = state.services.read().unwrap();
        match services.get(&id) {
            Some(s) if s.name == name => s.clone(),
            Some(_) => {
                return (StatusCode::NOT_FOUND, "service name/id mismatch").into_response();
            }
            None => {
                return (StatusCode::NOT_FOUND, "service not found").into_response();
            }
        }
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

    let upstream = state
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
            deregister(&state, id);
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "switchelo=info".to_string()),
        )
        .init();

    let state = AppState {
        services: Arc::new(RwLock::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(0)),
        client: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/registry", post(register))
        .route("/unregistry", post(unregister))
        .route("/list", get(list_all))
        .route("/{name}/list", get(list_by_name))
        // All other paths go to the dynamic forwarding handler.
        .fallback(proxy)
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("switchelo listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
