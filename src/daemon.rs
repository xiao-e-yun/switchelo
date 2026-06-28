//! The core daemon: an in-memory service registry plus the operations that act
//! on it. This module is transport-agnostic — it knows nothing about HTTP or
//! the command line. Inputs (see [`crate::inputs`]) drive it by calling these
//! methods, which keeps the registry logic decoupled from how requests arrive.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How many *consecutive* forwarding failures a service may accumulate before
/// it is deregistered. A single success resets the count, so only a sustained
/// outage evicts the backend.
const MAX_FAILURES: u32 = 3;

/// A registered backend service.
#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    /// Backend base address, e.g. http://127.0.0.1:8081
    pub url: String,
    pub description: String,
    /// Consecutive forwarding failures; reset on any success.
    failures: u32,
}

/// Shared daemon state: the registry, an auto-increment id generator, and the
/// HTTP client used for forwarding. Cheap to clone (everything is behind `Arc`).
#[derive(Clone)]
pub struct Daemon {
    services: Arc<RwLock<HashMap<u64, Service>>>,
    next_id: Arc<AtomicU64>,
    /// Shared client used by the proxy input to forward requests.
    pub client: reqwest::Client,
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

impl Daemon {
    pub fn new() -> Self {
        // The proxy client must never hang on a backend that accepts the
        // connection but stalls: `connect_timeout` caps the dial, `read_timeout`
        // caps the gap between reads (so it bounds idle stalls without killing
        // legitimately long streams like SSE or large downloads).
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Daemon {
            services: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
            client,
        }
    }

    /// Register a backend service and return its id.
    ///
    /// Idempotent by `url`: a repeated report from the same backend reuses the
    /// existing id and refreshes its name/description. The trailing slash is
    /// normalized so `http://host:8081` and `http://host:8081/` are the same
    /// backend.
    /// Returns `Err` with a human-readable reason if the URL scheme is not one
    /// we can forward to (only `http://` and `https://` are supported).
    pub fn register(
        &self,
        name: String,
        url: String,
        description: String,
    ) -> Result<u64, String> {
        // Trim trailing slash to avoid double slashes when forwarding.
        let url = url.trim_end_matches('/').to_string();

        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!(
                "unsupported URL scheme (expected http:// or https://): {url}"
            ));
        }

        let mut services = self.services.write().unwrap();

        if let Some((&id, existing)) = services.iter_mut().find(|(_, s)| s.url == url) {
            existing.name = name.clone();
            existing.description = description;
            // A fresh report is a healthy signal — clear any stale failures.
            existing.failures = 0;
            tracing::info!(id, name = %name, url = %url, "service re-registered (existing id reused)");
            return Ok(id);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let service = Service {
            name: name.clone(),
            url: url.clone(),
            description,
            failures: 0,
        };
        services.insert(id, service);
        tracing::info!(id, name = %name, url = %url, "service registered");
        Ok(id)
    }

    /// Remove a service by id. Returns `true` if a service with that id existed.
    /// Callers log the reason (graceful unregister vs. forwarding failure).
    pub fn unregister(&self, id: u64) -> bool {
        self.services.write().unwrap().remove(&id).is_some()
    }

    /// Record a successful forward to `id`, clearing its failure streak.
    pub fn record_success(&self, id: u64) {
        if let Some(s) = self.services.write().unwrap().get_mut(&id) {
            s.failures = 0;
        }
    }

    /// Record a forwarding failure for `id`. Once the streak reaches
    /// [`MAX_FAILURES`] the service is deregistered; returns `true` in that case.
    pub fn record_failure(&self, id: u64) -> bool {
        let mut services = self.services.write().unwrap();
        let Some(s) = services.get_mut(&id) else {
            return false;
        };
        s.failures += 1;
        if s.failures < MAX_FAILURES {
            return false;
        }
        services.remove(&id);
        true
    }

    /// Snapshot of every registered service, sorted by id. Inputs shape this
    /// into whatever view they expose.
    pub fn snapshot(&self) -> Vec<(u64, Service)> {
        let services = self.services.read().unwrap();
        let mut list: Vec<(u64, Service)> = services.iter().map(|(id, s)| (*id, s.clone())).collect();
        list.sort_by_key(|(id, _)| *id);
        list
    }

    /// Look up a single service by id.
    pub fn get(&self, id: u64) -> Option<Service> {
        self.services.read().unwrap().get(&id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_only_after_consecutive_failures() {
        let daemon = Daemon::new();
        let id = daemon
            .register("api".into(), "http://127.0.0.1:1".into(), String::new())
            .unwrap();

        // Below the threshold the service survives.
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!daemon.record_failure(id));
            assert!(daemon.get(id).is_some());
        }

        // The Nth consecutive failure evicts it.
        assert!(daemon.record_failure(id));
        assert!(daemon.get(id).is_none());
    }

    #[test]
    fn rejects_non_http_schemes_but_accepts_https() {
        let daemon = Daemon::new();
        assert!(
            daemon
                .register("api".into(), "ftp://host/x".into(), String::new())
                .is_err()
        );
        assert!(
            daemon
                .register("api".into(), "https://host".into(), String::new())
                .is_ok()
        );
    }

    #[test]
    fn success_resets_the_failure_streak() {
        let daemon = Daemon::new();
        let id = daemon
            .register("api".into(), "http://127.0.0.1:1".into(), String::new())
            .unwrap();

        for _ in 0..MAX_FAILURES - 1 {
            assert!(!daemon.record_failure(id));
        }
        // A success clears the streak, so the next failures start counting over.
        daemon.record_success(id);
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!daemon.record_failure(id));
            assert!(daemon.get(id).is_some());
        }
    }
}
