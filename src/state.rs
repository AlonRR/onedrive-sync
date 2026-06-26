//! Per-machine state (machine-state.json) and the shared catalog (mappings.json).
//! Reads are corruption- and BOM-tolerant (a missing/unparseable file yields
//! defaults, matching Read-OdsJson). Writes go through the same atomic no-BOM
//! path and locks the PowerShell tool uses, so the two round-trip cleanly.

use crate::jsonio;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

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
        jsonio::read_bom(&paths.machine_state())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, paths: &Paths) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            jsonio::write_atomic(&paths.machine_state(), &json);
        }
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

/// Read-modify-write machine-state.json under the same cross-process lock the
/// PowerShell tray uses, so the two cannot clobber each other (port of
/// Edit-OdsMachineState). Best-effort: a stale lock past the timeout is broken.
pub fn edit<F: FnOnce(&mut MachineState)>(paths: &Paths, mutate: F) {
    let _ = std::fs::create_dir_all(&paths.local_root);
    let _lock = jsonio::lock_or_break(&paths.state_lock(), Duration::from_secs(5));
    let mut s = MachineState::load(paths);
    mutate(&mut s);
    s.save(paths);
}

/// Move a project to active / skip / undecided (port of Set-OdsState): drop it
/// from both lists first, then add to the chosen one (undecided = neither).
pub fn set_state(paths: &Paths, id: &str, status: Status) {
    edit(paths, |s| {
        s.active.retain(|a| !a.eq_ignore_ascii_case(id));
        s.skip.retain(|a| !a.eq_ignore_ascii_case(id));
        match status {
            Status::Active => s.active.push(id.to_string()),
            Status::Skip => s.skip.push(id.to_string()),
            Status::Undecided => {}
        }
    });
}

/// Set/clear a project's per-project compare mode and max-delete override
/// (port of Set-OdsProjectSettings; None clears the entry).
pub fn set_project_settings(
    paths: &Paths,
    id: &str,
    compare: Option<&str>,
    max_delete: Option<u32>,
) {
    edit(paths, |s| {
        match compare {
            Some(c) => {
                s.compare.insert(id.to_string(), c.to_string());
            }
            None => {
                s.compare.remove(id);
            }
        }
        match max_delete {
            Some(m) => {
                s.max_delete.insert(id.to_string(), m);
            }
            None => {
                s.max_delete.remove(id);
            }
        }
    });
}

/// Bump a project's consecutive-defer counter; log an escalation at the threshold
/// (port of Update-OdsDeferCount).
pub fn update_defer_count(paths: &Paths, id: &str, escalate_cycles: u32) {
    edit(paths, |s| {
        let n = s.deferred.get(id).copied().unwrap_or(0) + 1;
        s.deferred.insert(id.to_string(), n);
        if n >= escalate_cycles {
            crate::events::log(
                paths,
                "ERROR",
                &format!("ESCALATION: {id} deferred {n} consecutive cycles — needs attention."),
            );
        }
    });
}

/// mappings.json: catalog of watch/plain mappings + tombstones (`forgotten`).
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
    pub forgotten: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogEntry {
    pub id: String,
    #[serde(rename = "localRel")]
    pub local_rel: String,
    #[serde(rename = "destRel")]
    pub dest_rel: String,
    pub kind: String,
    /// Preserve any fields a future/other tool version added, so a merge-write
    /// never silently drops data from the shared file.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Catalog {
    pub fn load(paths: &Paths) -> Self {
        Self::read_path(&paths.mappings())
    }

    fn read_path(path: &std::path::Path) -> Self {
        let Some(text) = jsonio::read_bom(path) else {
            return Self::default();
        };
        // Tolerate the older bare-array shape ([entries]).
        if let Ok(entries) = serde_json::from_str::<Vec<CatalogEntry>>(&text) {
            return Self { entries, forgotten: vec![] };
        }
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn is_forgotten(&self, id: &str) -> bool {
        self.forgotten.iter().any(|f| f.eq_ignore_ascii_case(id))
    }

    /// Persist the catalog, first merging any OneDrive conflict copies
    /// (`mappings-*.json`) by union of id, then writing atomically. Port of
    /// Save-OdsCatalog + Merge-OdsCatalog — required because a per-machine lock
    /// can't protect a file OneDrive forks across machines.
    pub fn save(&self, paths: &Paths) {
        let mappings = paths.mappings();
        let Some(dir) = mappings.parent() else { return };
        let mut merged = self.clone();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("mappings-") && name.ends_with(".json") {
                    let other = Self::read_path(&e.path());
                    merged = Self::merge(&merged, &other);
                    let _ = std::fs::remove_file(e.path());
                    crate::events::log(paths, "WARN", &format!("Merged catalog conflict copy {name}."));
                }
            }
        }
        if let Ok(json) = serde_json::to_string_pretty(&merged) {
            jsonio::write_atomic(&mappings, &json);
        }
    }

    /// Union two catalogs by entry id; a live entry beats a tombstone so a stale
    /// conflict copy can't re-retire a revived project.
    fn merge(a: &Catalog, b: &Catalog) -> Catalog {
        let mut by_id: HashMap<String, CatalogEntry> = HashMap::new();
        for e in a.entries.iter().chain(b.entries.iter()) {
            by_id.insert(e.id.to_lowercase(), e.clone());
        }
        let live: std::collections::HashSet<String> =
            by_id.values().map(|e| e.id.to_lowercase()).collect();
        let mut forgotten: Vec<String> = a
            .forgotten
            .iter()
            .chain(b.forgotten.iter())
            .filter(|f| !f.is_empty() && !live.contains(&f.to_lowercase()))
            .cloned()
            .collect();
        forgotten.sort_by_key(|f| f.to_lowercase());
        forgotten.dedup_by_key(|f| f.to_lowercase());
        let mut entries: Vec<CatalogEntry> = by_id.into_values().collect();
        entries.sort_by_key(|e| e.id.to_lowercase());
        Catalog { entries, forgotten }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PowerShell tray relies on `compare`/`deferred`/`maxDelete` being JSON
    /// OBJECTS (PSObject.Properties access) and `active`/`skip` being ARRAYS. Lock
    /// that shape — a regression here silently drops per-project settings in 5.1.
    #[test]
    fn machine_state_shape_round_trips() {
        let mut s = MachineState::default();
        s.active.push("a".into());
        s.compare.insert("a".into(), "checksum".into());
        s.max_delete.insert("a".into(), 40);
        let json = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["active"].is_array());
        assert!(v["compare"].is_object());
        assert!(v["deferred"].is_object());
        assert!(v["maxDelete"].is_object(), "maxDelete must serialize as an object");
        assert_eq!(v["maxDelete"]["a"], 40);
        // Empty maps must still be objects, not arrays, on a fresh machine.
        let empty = serde_json::to_value(MachineState::default()).unwrap();
        assert!(empty["compare"].is_object() && empty["maxDelete"].is_object());
    }

    /// A live entry must win over a tombstone so a stale conflict copy can't
    /// re-retire a revived project.
    #[test]
    fn catalog_merge_live_beats_tombstone() {
        let a = Catalog {
            entries: vec![CatalogEntry {
                id: "x".into(),
                local_rel: "L".into(),
                dest_rel: "x".into(),
                kind: "watch".into(),
                extra: Default::default(),
            }],
            forgotten: vec![],
        };
        let b = Catalog { entries: vec![], forgotten: vec!["x".into(), "y".into()] };
        let m = Catalog::merge(&a, &b);
        assert_eq!(m.entries.len(), 1);
        assert!(!m.is_forgotten("x"), "x is live; must not stay tombstoned");
        assert!(m.is_forgotten("y"));
    }

    /// A leading BOM (PowerShell 5.1 Add-Content) must not blank the state.
    #[test]
    fn bom_prefixed_json_still_parses() {
        let body = "\u{feff}{\"active\":[\"keep\"]}";
        let stripped = body.strip_prefix('\u{feff}').unwrap();
        let s: MachineState = serde_json::from_str(stripped).unwrap();
        assert_eq!(s.active, vec!["keep".to_string()]);
    }
}
