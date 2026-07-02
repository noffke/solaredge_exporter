use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

use crate::config::Config;
use crate::metrics::{AppMetrics, BatteryLabels, MeterLabels, MonitoringEndpoint, NoLabels};
use crate::monitoring_api::client::{MonitoringApiClient, MonitoringApiError};

/// Bring the Prometheus counter up to the persisted value for each battery
/// before the HTTP server accepts any scrape. Call this synchronously, once,
/// at startup — *after* `MonitoringApiClient::new` has loaded the state
/// file, *before* spawning the refresh task or serving `/metrics`.
///
/// Prometheus sees a counter reset from whatever the previous process was
/// reporting down to 0 (new binary) and then up to the persisted value. The
/// standard `rate()`/`increase()` reset heuristic treats that as a single
/// reset with no spurious delta, so long-range queries span the restart
/// without double-counting.
pub fn seed_counter_from_state(client: &MonitoringApiClient, metrics: &AppMetrics) {
    let totals = client.persisted_battery_totals();
    if totals.is_empty() {
        return;
    }
    for (serial, total) in &totals {
        let labels = BatteryLabels {
            battery: serial.clone(),
            model: total.model.clone(),
        };
        metrics
            .battery_ac_grid_charging
            .get_or_create(&labels)
            .inc_by(total.ac_grid_charging_watt_hours);
    }
    info!(
        batteries = totals.len(),
        "seeded ac_grid_charging counter from persistent state"
    );
}

pub async fn run(client: Arc<MonitoringApiClient>, config: Arc<Config>, metrics: Arc<AppMetrics>) {
    let interval = Duration::from_secs(config.monitoring_api.refresh_seconds);
    // 3 endpoints per cycle. Surface the budget risk at startup so it maps to
    // the SolarEdgeApiBudgetHigh alert (which only trips from polling too fast).
    let est_requests_per_day = 3 * 86_400 / config.monitoring_api.refresh_seconds.max(1);
    if est_requests_per_day > 250 {
        warn!(
            refresh_seconds = config.monitoring_api.refresh_seconds,
            estimated_requests_per_day = est_requests_per_day,
            "monitoring_api.refresh_seconds is low: projected requests/day approaches the 300/day \
             hard cap (the SolarEdgeApiBudgetHigh alert may fire). Raise refresh_seconds — 900s keeps it ~144/day."
        );
    }
    loop {
        let start = Instant::now();
        refresh_once(&client, &metrics).await;
        info!(
            elapsed_secs = start.elapsed().as_secs_f64(),
            "monitoring_api refresh cycle complete"
        );
        tokio::time::sleep(interval).await;
    }
}

async fn refresh_once(client: &MonitoringApiClient, metrics: &AppMetrics) {
    // --- Phase 1: gather (async). -----------------------------------------
    let overview = record("overview", client.fetch_overview(), metrics).await;
    let meters = record("meters", client.fetch_meters(), metrics).await;
    let storage = record("storage", client.fetch_storage(), metrics).await;

    // --- Phase 2: commit (synchronous burst). -----------------------------
    if let Some(r) = overview.as_ref()
        && let Some(wh) = r.overview.life_time_data.energy
    {
        crate::metrics::set_lifetime_monotonic(
            &metrics.site_pv_lifetime_energy.get_or_create(&NoLabels {}),
            wh,
            "site_pv_lifetime",
        );
    }

    if let Some(r) = meters.as_ref() {
        for meter in &r.meter_energy_details.meters {
            let Some(value) = meter.latest_value() else {
                continue;
            };
            let labels = MeterLabels {
                meter: meter.meter_serial_number.clone(),
                inverter: meter.connected_solaredge_device_sn.clone(),
                r#type: meter.meter_type.clone(),
            };
            crate::metrics::set_lifetime_monotonic(
                &metrics
                    .monitoring_meter_lifetime_energy
                    .get_or_create(&labels),
                value,
                &format!("meter:{}", meter.meter_type),
            );
        }
    }

    if let Some(r) = storage.as_ref() {
        for battery in &r.storage_data.batteries {
            let labels = BatteryLabels {
                battery: battery.serial_number.clone(),
                model: battery.model_number.clone(),
            };
            // battery_energy_charged / battery_energy_discharged are now
            // site-level gauges populated from the portal dashboard energy
            // endpoint — the public storageData API reports lifeTimeEnergy*
            // as 0 for the SolarEdge Home Battery 48V. See src/scrape.rs.
            if let Some(v) = battery.latest(|t| t.ac_grid_charging)
                && v >= 0.0
            {
                // `ACGridCharging` is the sum over the exact window we
                // requested (tracked in `MonitoringApiClient`), so each cycle
                // contributes a non-overlapping delta that accumulates into
                // the Prometheus counter AND the persistent-state file.
                // Record even when `v == 0` so the battery shows up in state
                // immediately (important on sunny days where the first real
                // grid charging may be days away). `inc_by(0)` is a no-op
                // on the counter but creates the series at 0.
                client.record_grid_charging(&battery.serial_number, &battery.model_number, v);
                metrics
                    .battery_ac_grid_charging
                    .get_or_create(&labels)
                    .inc_by(v);
            }
            if let Some(v) = battery.latest(|t| t.full_pack_energy_available) {
                metrics
                    .battery_full_pack_energy
                    .get_or_create(&labels)
                    .set(v);
            }
            if let Some(v) = battery.latest(|t| t.battery_state) {
                metrics.battery_state.get_or_create(&labels).set(v as f64);
            }
        }

        // Persist the updated counter + advanced last_storage_end to disk so
        // a subsequent process restart resumes without losing accumulated
        // grid-charging. No-op when state_file is unset.
        client.persist_state();
    }
}

/// Increment the request counter, await the fetch, and update the duration /
/// error / last-refresh gauges accordingly. Returns `None` if the fetch failed
/// (the error is logged and the error counter is bumped).
async fn record<T>(
    endpoint: &'static str,
    fut: impl Future<Output = Result<T, MonitoringApiError>>,
    metrics: &AppMetrics,
) -> Option<T> {
    let labels = MonitoringEndpoint {
        endpoint: endpoint.to_string(),
    };
    metrics.monitoring_api_requests.get_or_create(&labels).inc();
    let start = Instant::now();
    match fut.await {
        Ok(v) => {
            let now = jiff::Timestamp::now().as_second() as f64;
            metrics
                .monitoring_api_last_refresh
                .get_or_create(&labels)
                .set(now);
            metrics
                .monitoring_api_refresh_duration
                .get_or_create(&labels)
                .set(start.elapsed().as_secs_f64());
            Some(v)
        }
        Err(e) => {
            warn!(
                endpoint,
                error = %e,
                "monitoring_api fetch failed or returned no usable data; this endpoint's data will \
                 go stale (its refresh-error and staleness alerts will fire). EmptyResponse errors \
                 include the response body for diffing."
            );
            metrics
                .monitoring_api_refresh_errors
                .get_or_create(&labels)
                .inc();
            None
        }
    }
}
