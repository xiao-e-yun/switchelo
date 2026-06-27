//! The core daemon: an in-memory service registry plus the operations that act
//! on it. This module is transport-agnostic — it knows nothing about HTTP or
//! the command line. Inputs (see [`crate::inputs`]) drive it by calling these
//! methods, which keeps the registry logic decoupled from how requests arrive.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// A registered backend service.
#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    /// Backend base address, e.g. http://127.0.0.1:8081
    pub url: String,
    pub description: String,
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
        Daemon {
            services: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
            client: reqwest::Client::new(),
        }
    }

    /// Register a backend service and return its id.
    ///
    /// Idempotent by `url`: a repeated report from the same backend reuses the
    /// existing id and refreshes its name/description. The trailing slash is
    /// normalized so `http://host:8081` and `http://host:8081/` are the same
    /// backend.
    pub fn register(&self, name: String, url: String, description: String) -> u64 {
        // Trim trailing slash to avoid double slashes when forwarding.
        let url = url.trim_end_matches('/').to_string();

        let mut services = self.services.write().unwrap();

        if let Some((&id, existing)) = services.iter_mut().find(|(_, s)| s.url == url) {
            existing.name = name.clone();
            existing.description = description;
            tracing::info!(id, name = %name, url = %url, "service re-registered (existing id reused)");
            return id;
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let service = Service {
            name: name.clone(),
            url: url.clone(),
            description,
        };
        services.insert(id, service);
        tracing::info!(id, name = %name, url = %url, "service registered");
        id
    }

    /// Remove a service by id. Returns `true` if a service with that id existed.
    /// Callers log the reason (graceful unregister vs. forwarding failure).
    pub fn unregister(&self, id: u64) -> bool {
        self.services.write().unwrap().remove(&id).is_some()
    }

    /// Snapshot of all services as a map of `name -> description`. Multiple
    /// instances sharing a name collapse into one entry.
    pub fn list(&self) -> HashMap<String, String> {
        let services = self.services.read().unwrap();
        services
            .values()
            .map(|s| (s.name.clone(), s.description.clone()))
            .collect()
    }

    /// All instances registered under `name`, sorted by id.
    pub fn list_by_name(&self, name: &str) -> Vec<(u64, Service)> {
        let services = self.services.read().unwrap();
        let mut list: Vec<(u64, Service)> = services
            .iter()
            .filter(|(_, s)| s.name == name)
            .map(|(id, s)| (*id, s.clone()))
            .collect();
        list.sort_by_key(|(id, _)| *id);
        list
    }

    /// Look up a single service by id.
    pub fn get(&self, id: u64) -> Option<Service> {
        self.services.read().unwrap().get(&id).cloned()
    }
}
