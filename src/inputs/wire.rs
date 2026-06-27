//! The JSON contract shared by the HTTP server ([`super::http`]) and the CLI
//! client ([`super::cli`]). Defining these once means the client deserializes
//! exactly what the server serializes — no hand-rolled field plucking.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// `POST /registry` request body.
#[derive(Serialize, Deserialize)]
pub struct RegisterReq {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub description: String,
}

/// `POST /registry` response body.
#[derive(Serialize, Deserialize)]
pub struct RegisterRes {
    pub id: u64,
}

/// `POST /unregistry` request body.
#[derive(Serialize, Deserialize)]
pub struct UnregisterReq {
    pub id: u64,
}

/// `POST /unregistry` response body.
#[derive(Serialize, Deserialize)]
pub struct UnregisterRes {
    pub success: bool,
}

/// One backend instance in a [`ServiceGroup`].
#[derive(Serialize, Deserialize)]
pub struct Instance {
    pub url: String,
}

/// `GET /list` value: a named service and all instances running under it.
#[derive(Serialize, Deserialize)]
pub struct ServiceGroup {
    pub description: String,
    /// `id -> instance`.
    pub services: HashMap<u64, Instance>,
}
