use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub site_id: u64,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub refresh: RefreshConfig,
    #[serde(default)]
    pub monitoring_api: MonitoringApiConfig,
    #[serde(default)]
    pub fields: Vec<Field>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8888)),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RefreshConfig {
    /// Minimum seconds between refreshes of live optimizer telemetry.
    pub optimizer_seconds: u64,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            optimizer_seconds: 900,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MonitoringApiConfig {
    /// Seconds between calls to the public Monitoring API. 300 req/day hard
    /// cap × 3 endpoints per cycle ⇒ keep this ≥ 900 s in practice.
    pub refresh_seconds: u64,
    /// Path to the persistent-state JSON file (holds the
    /// `battery_ac_grid_charging` counter and the last `storageData` query
    /// window's end timestamp). Written atomically on every successful
    /// storage fetch. `None` ⇒ runtime-only accumulation (counter resets on
    /// restart); a WARN is logged at startup in that case. In Docker, mount
    /// a volume over the parent directory so the file survives restarts.
    #[serde(default)]
    pub state_file: Option<PathBuf>,
}

impl Default for MonitoringApiConfig {
    fn default() -> Self {
        Self {
            refresh_seconds: 1800,
            state_file: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(default)]
    pub optimizer_serials: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("field name {0:?} appears more than once")]
    DuplicateFieldName(String),
    #[error("optimizer serial {serial:?} is listed in multiple fields ({first:?}, {second:?})")]
    DuplicateSerial {
        serial: String,
        first: String,
        second: String,
    },
    #[error("refresh.optimizer_seconds must be > 0")]
    ZeroRefreshInterval,
    #[error("monitoring_api.refresh_seconds must be > 0")]
    ZeroMonitoringApiInterval,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let body = std::fs::read_to_string(path_ref).map_err(|e| ConfigError::Io {
            path: path_ref.display().to_string(),
            source: e,
        })?;
        let cfg: Config = toml::from_str(&body).map_err(|e| ConfigError::Parse {
            path: path_ref.display().to_string(),
            source: e,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.refresh.optimizer_seconds == 0 {
            return Err(ConfigError::ZeroRefreshInterval);
        }
        if self.monitoring_api.refresh_seconds == 0 {
            return Err(ConfigError::ZeroMonitoringApiInterval);
        }
        let mut seen_names: HashSet<&str> = HashSet::new();
        for f in &self.fields {
            if !seen_names.insert(f.name.as_str()) {
                return Err(ConfigError::DuplicateFieldName(f.name.clone()));
            }
        }
        let mut seen_serials: HashMap<String, String> = HashMap::new();
        for f in &self.fields {
            for s in &f.optimizer_serials {
                if let Some(first) = seen_serials.get(s) {
                    return Err(ConfigError::DuplicateSerial {
                        serial: s.clone(),
                        first: first.clone(),
                        second: f.name.clone(),
                    });
                }
                seen_serials.insert(s.clone(), f.name.clone());
            }
        }
        Ok(())
    }

    /// Returns the field name assigned to a given optimizer serial, or
    /// `"unassigned"` if the serial is not mapped.
    pub fn field_for(&self, serial: &str) -> &str {
        for f in &self.fields {
            if f.optimizer_serials.iter().any(|s| s == serial) {
                return &f.name;
            }
        }
        "unassigned"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_minimal_config() {
        let toml = r#"
            site_id = 42
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.site_id, 42);
        assert_eq!(cfg.refresh.optimizer_seconds, 900);
        assert_eq!(cfg.monitoring_api.refresh_seconds, 1800);
        assert_eq!(cfg.server.listen.port(), 8888);
        assert!(cfg.fields.is_empty());
    }

    #[test]
    fn loads_full_config() {
        let toml = r#"
            site_id = 42

            [server]
            listen = "127.0.0.1:9090"

            [refresh]
            optimizer_seconds = 600

            [[fields]]
            name = "east"
            description = "east facing"
            optimizer_serials = ["A", "B"]

            [[fields]]
            name = "south"
            optimizer_serials = ["C"]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.field_for("A"), "east");
        assert_eq!(cfg.field_for("C"), "south");
        assert_eq!(cfg.field_for("Z"), "unassigned");
    }

    #[test]
    fn rejects_duplicate_serial() {
        let toml = r#"
            site_id = 1
            [[fields]]
            name = "a"
            optimizer_serials = ["X"]
            [[fields]]
            name = "b"
            optimizer_serials = ["X"]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let err = cfg.validate().expect_err("duplicate serial");
        assert!(matches!(err, ConfigError::DuplicateSerial { .. }));
    }

    #[test]
    fn rejects_duplicate_field_name() {
        let toml = r#"
            site_id = 1
            [[fields]]
            name = "a"
            optimizer_serials = ["X"]
            [[fields]]
            name = "a"
            optimizer_serials = ["Y"]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let err = cfg.validate().expect_err("duplicate name");
        assert!(matches!(err, ConfigError::DuplicateFieldName(_)));
    }
}
