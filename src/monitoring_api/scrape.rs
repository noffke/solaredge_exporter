use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

use crate::config::Config;
use crate::metrics::{AppMetrics, BatteryLabels, MeterLabels, MonitoringEndpoint};
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
        metrics.site_pv_lifetime_energy.set(wh);
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
            metrics
                .monitoring_meter_lifetime_energy
                .get_or_create(&labels)
                .set(value);
        }
    }

    if let Some(r) = storage.as_ref() {
        for battery in &r.storage_data.batteries {
            let Some(tele) = battery.latest_telemetry() else {
                continue;
            };
            let labels = BatteryLabels {
                battery: battery.serial_number.clone(),
                model: battery.model_number.clone(),
            };
            if let Some(v) = tele.life_time_energy_charged {
                metrics.battery_energy_charged.get_or_create(&labels).set(v);
            }
            if let Some(v) = tele.life_time_energy_discharged {
                metrics
                    .battery_energy_discharged
                    .get_or_create(&labels)
                    .set(v);
            }
            if let Some(v) = tele.ac_grid_charging
                && v > 0.0
            {
                // `ACGridCharging` is the sum over the exact window we
                // requested (tracked in `MonitoringApiClient`), so each cycle
                // contributes a non-overlapping delta that accumulates into
                // the Prometheus counter AND the persistent-state file.
                client.record_grid_charging(&battery.serial_number, &battery.model_number, v);
                metrics
                    .battery_ac_grid_charging
                    .get_or_create(&labels)
                    .inc_by(v);
            }
            if let Some(v) = tele.full_pack_energy_available {
                metrics
                    .battery_full_pack_energy
                    .get_or_create(&labels)
                    .set(v);
            }
            if let Some(v) = tele.state_of_charge {
                metrics
                    .battery_state_of_charge
                    .get_or_create(&labels)
                    .set(v);
            }
            if let Some(v) = tele.power {
                metrics.battery_power.get_or_create(&labels).set(v);
            }
            if let Some(v) = tele.internal_temp {
                metrics.battery_internal_temp.get_or_create(&labels).set(v);
            }
            if let Some(v) = tele.battery_state {
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
            warn!(endpoint, error = %e, "monitoring_api fetch failed");
            metrics
                .monitoring_api_refresh_errors
                .get_or_create(&labels)
                .inc();
            None
        }
    }
}
