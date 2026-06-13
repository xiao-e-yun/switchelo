use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// 一筆已註冊的後端服務。
#[derive(Clone, Debug)]
struct Service {
    name: String,
    /// 後端基底位址，例如 http://127.0.0.1:8081
    url: String,
    #[allow(dead_code)]
    description: String,
}

/// 共享狀態：註冊表 + 自增 id 產生器 + HTTP client。
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

/// POST /registry —— 服務主動報到。
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Json<RegisterRes> {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let service = Service {
        name: req.name.clone(),
        // 去掉尾端斜線，避免轉發時出現雙斜線。
        url: req.url.trim_end_matches('/').to_string(),
        description: req.description,
    };
    state.services.write().unwrap().insert(id, service);
    tracing::info!(id, name = %req.name, url = %req.url, "service registered");
    Json(RegisterRes { success: true, id })
}

/// POST /unregistry —— 服務優雅下線。
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

/// 主動除名：轉發失敗時自動移除該服務。
fn deregister(state: &AppState, id: u64) {
    if state.services.write().unwrap().remove(&id).is_some() {
        tracing::warn!(id, "service deregistered due to forwarding failure");
    }
}

/// 依路徑轉發：GET/POST/... /{name}/{id}/...
/// 會剝離 /{name}/{id} 前綴後轉發到後端基底位址。
async fn proxy(State(state): State<AppState>, req: Request) -> Response {
    // 從路徑手動解析 name 與 id（避免 Path extractor 在 wildcard/trailing-slash
    // 情境下的參數數量不一致問題）。
    let path = req.uri().path().to_string();
    let mut segs = path.splitn(4, '/').skip(1); // 去掉開頭空字串
    let name = segs.next().unwrap_or("").to_string();
    let id: u64 = match segs.next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return (StatusCode::NOT_FOUND, "invalid path").into_response(),
    };

    // 查表取得目標服務。
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

    // 計算剝離前綴後的剩餘路徑。
    let prefix = format!("/{name}/{id}");
    let rest = path.strip_prefix(&prefix).unwrap_or("");
    let rest = if rest.is_empty() { "/" } else { rest };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!("{}{}{}", service.url, rest, query);

    // 重組請求送往後端。
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
            // 連不上後端 -> 主動除名。
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
        // 其餘所有路徑都進入動態轉發處理。
        .fallback(proxy)
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("switchelo listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
