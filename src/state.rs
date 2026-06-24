//! Per-machine state (machine-state.json) and the shared catalog (mappings.json).
//! Reads are corruption-tolerant: a missing or unparseable file yields defaults,
//! matching Read-OdsJson in the PowerShell tool.

use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Skip,
    Undecided,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Skip => "skip",
            Status::Undecided => "undecided",
        }
    }
}

/// machine-state.json: `active`/`skip` are id arrays; `compare`/`deferred`/
/// `maxDelete` are objects keyed by id.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MachineState {
    pub active: Vec<String>,
    pub skip: Vec<String>,
    pub compare: HashMap<String, String>,
    pub deferred: HashMap<String, u32>,
    #[serde(rename = "maxDelete")]
    pub max_delete: HashMap<String, u32>,
}

impl MachineState {
    pub fn load(paths: &Paths) -> Self {
        std::fs::read_to_string(paths.machine_state())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Case-insensitive status lookup (ids are matched case-insensitively).
    pub fn status_of(&self, id: &str) -> Status {
        if self.active.iter().any(|a| a.eq_ignore_ascii_case(id)) {
            Status::Active
        } else if self.skip.iter().any(|s| s.eq_ignore_ascii_case(id)) {
            Status::Skip
        } else {
            Status::Undecided
        }
    }
}

/// mappings.json: catalog of watch/plain mappings + tombstones (`forgotten`).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
    pub forgotten: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    #[serde(rename = "localRel")]
    pub local_rel: String,
    #[serde(rename = "destRel")]
    pub dest_rel: String,
    pub kind: String,
}

impl Catalog {
    pub fn load(paths: &Paths) -> Self {
        std::fs::read_to_string(paths.mappings())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn is_forgotten(&self, id: &str) -> bool {
        self.forgotten.iter().any(|f| f.eq_ignore_ascii_case(id))
    }
}
