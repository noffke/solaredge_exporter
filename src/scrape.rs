use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::metrics::{AppMetrics, OptimizerLabels, RefreshKind};
use crate::portal::{FlatOptimizer, PortalClient};

pub async fn run(
    client: Arc<PortalClient>,
    config: Arc<Config>,
    optimizers: Arc<Vec<FlatOptimizer>>,
    metrics: Arc<AppMetrics>,
) {
    let interval = Duration::from_secs(config.refresh.optimizer_seconds);
    loop {
        let start = Instant::now();
        refresh_once(&client, &config, &optimizers, &metrics).await;
        let elapsed = start.elapsed().as_secs_f64();
        metrics
            .refresh_duration
            .get_or_create(&RefreshKind {
                kind: "telemetry".into(),
            })
            .set(elapsed);
        info!(elapsed_secs = elapsed, "refresh cycle complete");
        tokio::time::sleep(interval).await;
    }
}

/// Per-optimizer readings buffered during the gather phase. Everything is
/// `Option<f64>` so `None` entries can simply be skipped during commit without
/// touching existing gauge values.
#[derive(Default)]
struct OptimizerReading {
    power: Option<f64>,
    module_voltage: Option<f64>,
    dc_voltage: Option<f64>,
    current: Option<f64>,
    energy_today: Option<f64>,
    last_measurement: Option<f64>,
}

async fn refresh_once(
    client: &PortalClient,
    config: &Config,
    optimizers: &[FlatOptimizer],
    metrics: &AppMetrics,
) {
    // --- Phase 1: gather (async, ~10s of HTTP, no gauge writes). ---------
    // Buffer all readings in memory so a concurrent /metrics scrape never
    // sees a half-applied refresh.
    let energy = fetch_energy_with_metrics(client, metrics).await;

    let mut readings: Vec<(OptimizerLabels, OptimizerReading)> =
        Vec::with_capacity(optimizers.len());
    for opt in optimizers {
        let labels = make_labels(opt, config);
        let mut reading = OptimizerReading::default();

        if let Some(energy) = energy.as_ref()
            && let Some(entry) = energy.get(&opt.reporter_id.to_string())
        {
            reading.energy_today = entry.watt_hours();
        }

        match client.fetch_optimizer(opt.reporter_id).await {
            Ok(Some(data)) => {
                reading.power = data.power_watts();
                reading.module_voltage = data.voltage_volts();
                reading.dc_voltage = data.optimizer_voltage_volts();
                reading.current = data.current_amps();
                reading.last_measurement =
                    parse_last_measurement(&data.last_measurement_date).map(|ts| ts as f64);
            }
            Ok(None) => {
                debug!(
                    optimizer = %opt.serial_number,
                    "optimizer has no measurements yet"
                );
            }
            Err(e) => {
                warn!(
                    optimizer = %opt.serial_number,
                    error = %e,
                    "fetch_optimizer failed"
                );
                metrics
                    .refresh_errors
                    .get_or_create(&RefreshKind {
                        kind: "optimizer".into(),
                    })
                    .inc();
            }
        }

        readings.push((labels, reading));
    }

    // --- Phase 2: commit (synchronous burst, microseconds). --------------
    // Flushing in a tight loop with no awaits and no fallible ops keeps the
    // window during which /metrics could see a mix of old and new values to
    // the bare minimum.
    for (labels, r) in &readings {
        if let Some(v) = r.power {
            metrics.power.get_or_create(labels).set(v);
        }
        if let Some(v) = r.module_voltage {
            metrics.module_voltage.get_or_create(labels).set(v);
        }
        if let Some(v) = r.dc_voltage {
            metrics.dc_voltage.get_or_create(labels).set(v);
        }
        if let Some(v) = r.current {
            metrics.current.get_or_create(labels).set(v);
        }
        if let Some(v) = r.energy_today {
            metrics.energy_today.get_or_create(labels).set(v);
        }
        if let Some(v) = r.last_measurement {
            metrics.last_measurement.get_or_create(labels).set(v);
        }
    }

    let now = jiff::Timestamp::now().as_second() as f64;
    metrics
        .last_refresh
        .get_or_create(&RefreshKind {
            kind: "telemetry".into(),
        })
        .set(now);
}

async fn fetch_energy_with_metrics(
    client: &PortalClient,
    metrics: &AppMetrics,
) -> Option<crate::portal::EnergyResponse> {
    let start = Instant::now();
    metrics.login_count.inc();
    match client.fetch_energy().await {
        Ok(e) => {
            let now = jiff::Timestamp::now().as_second() as f64;
            metrics
                .last_refresh
                .get_or_create(&RefreshKind {
                    kind: "energy".into(),
                })
                .set(now);
            metrics
                .refresh_duration
                .get_or_create(&RefreshKind {
                    kind: "energy".into(),
                })
                .set(start.elapsed().as_secs_f64());
            Some(e)
        }
        Err(e) => {
            warn!(
                error = %e,
                "fetch_energy failed; continuing without lifetime energy"
            );
            metrics
                .refresh_errors
                .get_or_create(&RefreshKind {
                    kind: "energy".into(),
                })
                .inc();
            None
        }
    }
}

fn make_labels(opt: &FlatOptimizer, config: &Config) -> OptimizerLabels {
    OptimizerLabels {
        optimizer: opt.serial_number.clone(),
        display_name: opt.display_name.clone(),
        inverter: opt.inverter_serial.clone(),
        field: config.field_for(&opt.serial_number).to_string(),
    }
}

/// Parse the portal's `lastMeasurementDate` format, e.g.
/// `"Thu Apr 23 12:26:12 GMT 2026"`. The TZ abbreviation is **discarded**: in
/// practice the portal labels the string `GMT` but emits the site's local
/// wall-clock time (verified 2026-04 against a Europe/Berlin site, where GMT
/// timestamps were ~2 h in the future relative to the fetch). The Python
/// reference also drops the TZ and parses naive. We interpret the naive
/// datetime in the system TZ — the Docker image sets this to Europe/Berlin,
/// so metric timestamps align with the rest of the log output.
fn parse_last_measurement(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let month = month_to_num(parts[1])?;
    let day: i8 = parts[2].parse().ok()?;
    let time_parts: Vec<&str> = parts[3].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: i8 = time_parts[0].parse().ok()?;
    let minute: i8 = time_parts[1].parse().ok()?;
    let second: i8 = time_parts[2].parse().ok()?;
    let year: i16 = parts[5].parse().ok()?;
    let dt = jiff::civil::DateTime::new(year, month, day, hour, minute, second, 0).ok()?;
    let zoned = dt.to_zoned(jiff::tz::TimeZone::system()).ok()?;
    Some(zoned.timestamp().as_second())
}

fn month_to_num(m: &str) -> Option<i8> {
    match m {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_last_measurement_known_format() {
        // Parse a known civil datetime and check round-trip via the system TZ.
        // The TZ abbreviation is ignored — the naive datetime fields must
        // survive intact.
        let ts = parse_last_measurement("Sun Jun 21 12:00:00 CEST 2026").expect("parse");
        let back = jiff::Timestamp::from_second(ts)
            .expect("ts")
            .to_zoned(jiff::tz::TimeZone::system());
        assert_eq!(back.year(), 2026);
        assert_eq!(back.month(), 6);
        assert_eq!(back.day(), 21);
        assert_eq!(back.hour(), 12);
        assert_eq!(back.minute(), 0);
        assert_eq!(back.second(), 0);
    }

    #[test]
    fn parse_last_measurement_ignores_tz_label() {
        // Portal labels the string "GMT" but the time is actually local.
        // Parsing and rendering via the system TZ must round-trip the
        // wall-clock fields unchanged, regardless of what the label says.
        let ts =
            parse_last_measurement("Thu Apr 23 12:26:12 GMT 2026").expect("parse GMT-labelled");
        let back = jiff::Timestamp::from_second(ts)
            .expect("ts")
            .to_zoned(jiff::tz::TimeZone::system());
        assert_eq!(back.year(), 2026);
        assert_eq!(back.month(), 4);
        assert_eq!(back.day(), 23);
        assert_eq!(back.hour(), 12);
        assert_eq!(back.minute(), 26);
        assert_eq!(back.second(), 12);
    }

    #[test]
    fn parse_last_measurement_rejects_garbage() {
        assert!(parse_last_measurement("").is_none());
        assert!(parse_last_measurement("not a date").is_none());
        assert!(parse_last_measurement("Wed Foo 25 12:34:56 CET 2026").is_none());
    }
}
