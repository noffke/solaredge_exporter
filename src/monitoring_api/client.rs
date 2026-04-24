use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;

use reqwest::StatusCode;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::monitoring_api::models::{MetersResponse, OverviewResponse, StorageDataResponse};
use crate::monitoring_api::state::{self, BatteryState, PersistentState};
use crate::portal::Secret;

const BASE: &str = "https://monitoringapi.solaredge.com";
const USER_AGENT: &str = "solaredge_exporter/0.1.0";

#[derive(Debug, Error)]
pub enum MonitoringApiError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected HTTP {status} from {endpoint}: {body}")]
    Status {
        endpoint: &'static str,
        status: StatusCode,
        body: String,
    },
    #[error("JSON decode error from {endpoint}: {source}")]
    Json {
        endpoint: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to build HTTP client")]
    BuildClient(#[source] reqwest::Error),
    #[error("failed to format time window for request: {0}")]
    Time(String),
}

/// Per-battery persistent counter state held in memory between refreshes.
#[derive(Debug, Clone)]
pub struct BatteryTotal {
    pub model: String,
    pub ac_grid_charging_watt_hours: f64,
}

pub struct MonitoringApiClient {
    site_id: u64,
    api_key: Secret,
    http: reqwest::Client,
    state_file: Option<PathBuf>,
    /// End of the last successful `fetch_storage` window. Initialised from
    /// the state file on startup (or `None` on first run). Each `fetch_storage`
    /// uses this as the `startTime`, so windows are non-overlapping and the
    /// response's `ACGridCharging` is a proper delta.
    last_storage_end: Mutex<Option<jiff::Timestamp>>,
    /// Per-battery accumulator + model label. Seeded from the state file on
    /// startup; updated in place after every successful storage fetch, then
    /// written back to disk atomically by `persist_state()`.
    battery_totals: Mutex<HashMap<String, BatteryTotal>>,
}

impl MonitoringApiClient {
    pub fn new(
        site_id: u64,
        api_key: Secret,
        state_file: Option<PathBuf>,
    ) -> Result<Self, MonitoringApiError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(MonitoringApiError::BuildClient)?;

        let (last_end, totals) = load_state(state_file.as_deref());
        let client = Self {
            site_id,
            api_key,
            http,
            state_file,
            last_storage_end: Mutex::new(last_end),
            battery_totals: Mutex::new(totals),
        };
        Ok(client)
    }

    pub async fn fetch_overview(&self) -> Result<OverviewResponse, MonitoringApiError> {
        let url = format!("{BASE}/site/{}/overview", self.site_id);
        self.get_json(&url, None, "overview").await
    }

    pub async fn fetch_meters(&self) -> Result<MetersResponse, MonitoringApiError> {
        let (start, end) = time_window_days(2)?;
        let url = format!("{BASE}/site/{}/meters", self.site_id);
        let params: Vec<(&str, String)> = vec![
            ("startTime", start),
            ("endTime", end),
            ("timeUnit", "DAY".to_string()),
            (
                "meters",
                "Production,Consumption,FeedIn,Purchased".to_string(),
            ),
        ];
        self.get_json(&url, Some(&params), "meters").await
    }

    pub async fn fetch_storage(&self) -> Result<StorageDataResponse, MonitoringApiError> {
        // Query `[last_storage_end, now]`. On the very first call the cap is
        // 24 h (or whatever is left of the API's 7-day window limit if the
        // state file's timestamp was older than that).
        let now = jiff::Timestamp::now();
        let start_ts = {
            let guard = self
                .last_storage_end
                .lock()
                .expect("last_storage_end mutex poisoned");
            match *guard {
                Some(ts) => cap_to_seven_days(ts, now)?,
                None => {
                    let span = jiff::Span::new()
                        .try_hours(24)
                        .map_err(|e| MonitoringApiError::Time(e.to_string()))?;
                    now.checked_sub(span)
                        .map_err(|e| MonitoringApiError::Time(e.to_string()))?
                }
            }
        };

        let fmt = "%Y-%m-%d %H:%M:%S";
        let tz = jiff::tz::TimeZone::system();
        let start = start_ts.to_zoned(tz.clone()).strftime(fmt).to_string();
        let end = now.to_zoned(tz).strftime(fmt).to_string();

        let url = format!("{BASE}/site/{}/storageData", self.site_id);
        let params: Vec<(&str, String)> = vec![("startTime", start), ("endTime", end)];
        let resp = self.get_json(&url, Some(&params), "storage").await?;

        // Only advance the window on success.
        *self
            .last_storage_end
            .lock()
            .expect("last_storage_end mutex poisoned") = Some(now);
        Ok(resp)
    }

    /// Add `delta` Wh of grid-to-battery energy to the persistent counter for
    /// the given battery. Updates the in-memory `battery_totals` map; call
    /// `persist_state()` to write to disk.
    pub fn record_grid_charging(&self, serial: &str, model: &str, delta: f64) {
        let mut totals = self
            .battery_totals
            .lock()
            .expect("battery_totals mutex poisoned");
        let entry = totals
            .entry(serial.to_string())
            .or_insert_with(|| BatteryTotal {
                model: model.to_string(),
                ac_grid_charging_watt_hours: 0.0,
            });
        // Keep the model up to date in case it changes after a firmware update.
        if !model.is_empty() && entry.model != model {
            entry.model = model.to_string();
        }
        entry.ac_grid_charging_watt_hours += delta;
    }

    /// Snapshot of the currently-accumulated per-battery totals. Used by the
    /// scrape task at startup to seed the Prometheus counter to the
    /// persisted value before the HTTP server accepts any scrape.
    pub fn persisted_battery_totals(&self) -> HashMap<String, BatteryTotal> {
        self.battery_totals
            .lock()
            .expect("battery_totals mutex poisoned")
            .clone()
    }

    /// Write the in-memory state (last_storage_end + battery_totals) to the
    /// configured state file. No-op when `state_file` is `None`. Errors are
    /// logged at WARN and swallowed — the exporter keeps running with
    /// whatever is already in memory rather than dying on a disk glitch.
    pub fn persist_state(&self) {
        let Some(path) = self.state_file.as_ref() else {
            return;
        };
        let last_end = self
            .last_storage_end
            .lock()
            .expect("last_storage_end mutex poisoned")
            .map(|t| t.to_string());
        let totals = self
            .battery_totals
            .lock()
            .expect("battery_totals mutex poisoned");
        let batteries = totals
            .iter()
            .map(|(serial, t)| {
                (
                    serial.clone(),
                    BatteryState {
                        model: t.model.clone(),
                        ac_grid_charging_watt_hours: t.ac_grid_charging_watt_hours,
                    },
                )
            })
            .collect();
        let state = PersistentState {
            version: 1,
            last_storage_end: last_end,
            batteries,
        };
        if let Err(e) = state.save(path) {
            state::log_state_error(&e);
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        extra_params: Option<&[(&str, String)]>,
        endpoint: &'static str,
    ) -> Result<T, MonitoringApiError> {
        let mut req = self
            .http
            .get(url)
            .query(&[("api_key", self.api_key.expose())]);
        if let Some(params) = extra_params {
            req = req.query(params);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint,
            status = %status,
            body = text.as_str(),
            "monitoring API response"
        );
        if !status.is_success() {
            return Err(MonitoringApiError::Status {
                endpoint,
                status,
                body: truncate(&text),
            });
        }
        serde_json::from_str(&text).map_err(|e| MonitoringApiError::Json {
            endpoint,
            source: e,
        })
    }
}

/// Load persistent state from `path` if given. Logs and falls back to empty
/// state on any error so a corrupt or missing file doesn't stop the exporter.
fn load_state(
    path: Option<&std::path::Path>,
) -> (Option<jiff::Timestamp>, HashMap<String, BatteryTotal>) {
    let Some(path) = path else {
        warn!(
            "monitoring_api.state_file not set; AC grid-charging counter will reset on every restart"
        );
        return (None, HashMap::new());
    };
    let state = match PersistentState::load(path) {
        Ok(s) => s,
        Err(e) => {
            state::log_state_error(&e);
            return (None, HashMap::new());
        }
    };
    let last_end =
        state
            .last_storage_end
            .as_deref()
            .and_then(|s| match jiff::Timestamp::from_str(s) {
                Ok(ts) => Some(ts),
                Err(err) => {
                    warn!(
                        value = s,
                        error = %err,
                        "state file last_storage_end is unparseable; treating as first run"
                    );
                    None
                }
            });
    let totals: HashMap<String, BatteryTotal> = state
        .batteries
        .into_iter()
        .map(|(serial, bs)| {
            (
                serial,
                BatteryTotal {
                    model: bs.model,
                    ac_grid_charging_watt_hours: bs.ac_grid_charging_watt_hours,
                },
            )
        })
        .collect();
    info!(
        path = %path.display(),
        last_storage_end = ?last_end,
        batteries = totals.len(),
        "loaded monitoring_api state"
    );
    (last_end, totals)
}

/// The storageData endpoint caps the query window at 7 days; if the state
/// file's last_end is older than that, clamp and log. Any AC grid charging
/// that happened in the clamped-off prefix is unrecoverable from the API.
fn cap_to_seven_days(
    last_end: jiff::Timestamp,
    now: jiff::Timestamp,
) -> Result<jiff::Timestamp, MonitoringApiError> {
    let span = jiff::Span::new()
        .try_hours(7 * 24)
        .map_err(|e| MonitoringApiError::Time(e.to_string()))?;
    let seven_days_ago = now
        .checked_sub(span)
        .map_err(|e| MonitoringApiError::Time(e.to_string()))?;
    if last_end < seven_days_ago {
        warn!(
            last_end = %last_end,
            seven_days_ago = %seven_days_ago,
            "last_storage_end is older than the API's 7-day window cap; grid charging between {last_end} and {seven_days_ago} will not be counted"
        );
        Ok(seven_days_ago)
    } else {
        Ok(last_end)
    }
}

fn time_window_days(days: i64) -> Result<(String, String), MonitoringApiError> {
    let fmt = "%Y-%m-%d %H:%M:%S";
    let tz = jiff::tz::TimeZone::system();
    let now = jiff::Timestamp::now();
    let end = now.to_zoned(tz.clone()).strftime(fmt).to_string();
    let span = jiff::Span::new()
        .try_hours(days * 24)
        .map_err(|e| MonitoringApiError::Time(e.to_string()))?;
    let start_ts = now
        .checked_sub(span)
        .map_err(|e| MonitoringApiError::Time(e.to_string()))?;
    let start = start_ts.to_zoned(tz).strftime(fmt).to_string();
    Ok((start, end))
}

fn truncate(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_window_is_well_formed() {
        let (start, end) = time_window_days(1).expect("window");
        assert_eq!(start.len(), 19, "YYYY-MM-DD HH:MM:SS");
        assert_eq!(end.len(), 19);
        assert!(start < end, "start < end: {start} < {end}");
    }

    #[test]
    fn cap_to_seven_days_clamps_old_timestamps() {
        let now = jiff::Timestamp::now();
        let ten_days_ago = now
            .checked_sub(jiff::Span::new().try_hours(10 * 24).unwrap())
            .unwrap();
        let capped = cap_to_seven_days(ten_days_ago, now).expect("cap");
        let seven_days_ago = now
            .checked_sub(jiff::Span::new().try_hours(7 * 24).unwrap())
            .unwrap();
        assert_eq!(capped, seven_days_ago);
    }

    #[test]
    fn cap_to_seven_days_preserves_recent_timestamps() {
        let now = jiff::Timestamp::now();
        let two_days_ago = now
            .checked_sub(jiff::Span::new().try_hours(2 * 24).unwrap())
            .unwrap();
        let capped = cap_to_seven_days(two_days_ago, now).expect("cap");
        assert_eq!(capped, two_days_ago);
    }

    #[test]
    fn record_grid_charging_accumulates() {
        let client = MonitoringApiClient::new(1, Secret::new("k".into()), None).unwrap();
        client.record_grid_charging("B1", "M", 100.0);
        client.record_grid_charging("B1", "M", 50.5);
        client.record_grid_charging("B2", "M2", 10.0);
        let totals = client.persisted_battery_totals();
        assert_eq!(totals["B1"].ac_grid_charging_watt_hours, 150.5);
        assert_eq!(totals["B2"].ac_grid_charging_watt_hours, 10.0);
        assert_eq!(totals["B2"].model, "M2");
    }
}
