use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

/// On-disk format. Bumped when the schema changes so a future reader can
/// refuse to read an incompatible version.
const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct PersistentState {
    #[serde(default = "default_version")]
    pub version: u32,
    /// RFC 3339 timestamp of the end of the last successful `storageData`
    /// query window. Used as the start of the next query so every interval
    /// is non-overlapping.
    #[serde(default)]
    pub last_storage_end: Option<String>,
    #[serde(default)]
    pub batteries: HashMap<String, BatteryState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BatteryState {
    /// Battery model (persisted so the Prometheus counter can re-use the
    /// same `{model=…}` label value across restarts — otherwise the seeded
    /// counter and the post-fetch counter end up on different series).
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub ac_grid_charging_watt_hours: f64,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            last_storage_end: None,
            batteries: HashMap::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to read state file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write state file {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("state file {path} has unexpected version {version}; refusing to proceed")]
    VersionMismatch { path: String, version: u32 },
    #[error("failed to parse state file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize state: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl PersistentState {
    /// Load state from the given path. A missing file yields an empty state
    /// (first run); any other I/O or parse error surfaces to the caller.
    pub fn load(path: &Path) -> Result<Self, StateError> {
        let body = match fs::read_to_string(path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(StateError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let state: Self = serde_json::from_str(&body).map_err(|source| StateError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        if state.version != CURRENT_VERSION {
            return Err(StateError::VersionMismatch {
                path: path.display().to_string(),
                version: state.version,
            });
        }
        Ok(state)
    }

    /// Atomic save: serialise, write to a sibling tempfile, rename into place.
    /// A crash mid-write leaves the previous state intact.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = sibling_tempfile(path);
        fs::write(&tmp, &bytes).map_err(|source| StateError::Write {
            path: tmp.display().to_string(),
            source,
        })?;
        if let Err(source) = fs::rename(&tmp, path) {
            // Best-effort cleanup; don't mask the rename error.
            let _ = fs::remove_file(&tmp);
            return Err(StateError::Write {
                path: path.display().to_string(),
                source,
            });
        }
        Ok(())
    }
}

fn sibling_tempfile(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|f| f.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Log at WARN and swallow state-file errors. State is a "nice to have" —
/// we'd rather keep the exporter running with a runtime-only counter than
/// bail the whole process because the disk is read-only or missing.
pub fn log_state_error(err: &StateError) {
    warn!(error = %err, "state file error; continuing with runtime-only accumulation");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_yields_empty_state() {
        let dir = tempdir();
        let path = dir.join("missing.json");
        let state = PersistentState::load(&path).expect("missing = empty");
        assert_eq!(state.version, CURRENT_VERSION);
        assert!(state.last_storage_end.is_none());
        assert!(state.batteries.is_empty());
    }

    #[test]
    fn round_trip_preserves_fields() {
        let dir = tempdir();
        let path = dir.join("state.json");
        let mut batteries = HashMap::new();
        batteries.insert(
            "BAT1".into(),
            BatteryState {
                model: "SolarEdge Home Battery 48V".into(),
                ac_grid_charging_watt_hours: 12345.5,
            },
        );
        let state = PersistentState {
            version: CURRENT_VERSION,
            last_storage_end: Some("2026-04-24T10:30:00Z".into()),
            batteries,
        };
        state.save(&path).expect("save");
        let reloaded = PersistentState::load(&path).expect("load");
        assert_eq!(
            reloaded.last_storage_end.as_deref(),
            Some("2026-04-24T10:30:00Z")
        );
        assert_eq!(
            reloaded.batteries["BAT1"].ac_grid_charging_watt_hours,
            12345.5
        );
        assert_eq!(
            reloaded.batteries["BAT1"].model,
            "SolarEdge Home Battery 48V"
        );
    }

    #[test]
    fn rejects_unknown_version() {
        let dir = tempdir();
        let path = dir.join("future.json");
        fs::write(&path, r#"{"version": 9999, "batteries": {}}"#).unwrap();
        let err = PersistentState::load(&path).expect_err("reject");
        assert!(matches!(
            err,
            StateError::VersionMismatch { version: 9999, .. }
        ));
    }

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("solaredge_exporter_test_{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        base
    }
}
