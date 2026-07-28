use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::metrics::{AppMetrics, NoLabels, OptimizerLabels, RefreshKind};
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

/// Max concurrent per-optimizer energy requests. The energy-graph endpoint
/// reports one scalar per call, so it needs one request per optimizer; keep the
/// fan-out modest rather than firing all of them at the portal at once.
const ENERGY_CONCURRENCY: usize = 6;

async fn refresh_once(
    client: &Arc<PortalClient>,
    config: &Config,
    optimizers: &[FlatOptimizer],
    metrics: &AppMetrics,
) {
    // --- Phase 1: gather (async, ~10s of HTTP, no gauge writes). ---------
    // Buffer all readings in memory so a concurrent /metrics scrape never
    // sees a half-applied refresh.

    // Every `/services/` call needs the Cognito token, and each one would mint
    // it on demand. Warm it once up front so `login_count` can record real SRP
    // handshakes (~1/day, the token lives ~24 h) instead of per-request cache
    // hits. A failure here is not fatal on its own — the individual fetches
    // below will fail and report against their own error kinds.
    match client.warm_services_auth().await {
        Ok(true) => {
            metrics.login_count.inc();
        }
        Ok(false) => {}
        Err(e) => warn!(error = %e, "Cognito SRP login failed; this cycle's fetches will fail"),
    }

    let serials: Vec<String> = optimizers.iter().map(|o| o.serial_number.clone()).collect();

    // One batch call covers every optimizer's live telemetry.
    let live = if serials.is_empty() {
        None
    } else {
        fetch_live_with_metrics(client, metrics, &serials).await
    };

    let energy = fetch_energy_with_metrics(client, metrics, &serials).await;

    let battery_energy = match client.fetch_battery_energy().await {
        Ok(e) => Some(e),
        Err(e) => {
            warn!(
                error = %e,
                "fetch_battery_energy failed or returned no usable data; battery charge/discharge \
                 will go stale (the 'battery_energy' staleness alert will fire). The error includes \
                 the response body when the dashboard schema drifted."
            );
            metrics
                .refresh_errors
                .get_or_create(&RefreshKind {
                    kind: "battery_energy".into(),
                })
                .inc();
            None
        }
    };

    let mut readings: Vec<(OptimizerLabels, OptimizerReading)> =
        Vec::with_capacity(optimizers.len());
    for opt in optimizers {
        let labels = make_labels(opt, config);
        let mut reading = OptimizerReading {
            energy_today: energy.get(&opt.serial_number).copied().flatten(),
            ..Default::default()
        };

        match live
            .as_ref()
            .and_then(|l| l.serial_to_live_data.get(&opt.serial_number))
        {
            Some(data) => {
                if !data.has_measurements() && opt.is_active() {
                    debug!(
                        optimizer = %opt.serial_number,
                        last_measurement = %data.last_measurement,
                        "live entry carried no electrical values"
                    );
                }
                reading.power = data.power_w;
                reading.module_voltage = data.voltage_v;
                reading.dc_voltage = data.optimizer_voltage_v;
                reading.current = data.current_a;
                reading.last_measurement =
                    parse_last_measurement(&data.last_measurement).map(|ts| ts as f64);
            }
            None if live.is_some() => {
                // The batch succeeded but this serial carried no live entry.
                // Expected for a replaced/inactive optimizer, which stays in the
                // layout forever — don't warn about those every cycle.
                if opt.is_active() {
                    warn!(
                        optimizer = %opt.serial_number,
                        status = %opt.status,
                        "optimizer missing from the live-data batch"
                    );
                    metrics
                        .refresh_errors
                        .get_or_create(&RefreshKind {
                            kind: "optimizer".into(),
                        })
                        .inc();
                } else {
                    debug!(
                        optimizer = %opt.serial_number,
                        status = %opt.status,
                        "inactive optimizer reported no live data"
                    );
                }
            }
            // Batch call itself failed — already logged and counted once.
            None => {}
        }

        readings.push((labels, reading));
    }

    // --- Phase 2: commit (synchronous burst, microseconds). --------------
    // Flushing in a tight loop with no awaits and no fallible ops keeps the
    // window during which /metrics could see a mix of old and new values to
    // the bare minimum.
    // Did at least one optimizer yield a live electrical measurement this
    // cycle? Gates the telemetry `last_refresh` stamp below. We key off the
    // power/voltage/current values (not the HTTP call or `energy_today`, which
    // comes from the separate energy endpoint), so a silent schema drift on the
    // live-data response — HTTP 200 but `power_W`/`voltage_V` renamed, leaving
    // every value `None` — stops advancing the timestamp and trips the
    // staleness alert. Night-safe: the portal keeps returning the last
    // measurement (a real `Some` value) overnight rather than dropping it.
    let mut telemetry_committed = false;
    for (labels, r) in &readings {
        if let Some(v) = r.power {
            metrics.power.get_or_create(labels).set(v);
            telemetry_committed = true;
        }
        if let Some(v) = r.module_voltage {
            metrics.module_voltage.get_or_create(labels).set(v);
            telemetry_committed = true;
        }
        if let Some(v) = r.dc_voltage {
            metrics.dc_voltage.get_or_create(labels).set(v);
            telemetry_committed = true;
        }
        if let Some(v) = r.current {
            metrics.current.get_or_create(labels).set(v);
            telemetry_committed = true;
        }
        if let Some(v) = r.energy_today {
            // Despite the `_today` name this is a *lifetime* total (the retired
            // endpoint was queried with `timeUnit=ALL`; the replacement returns
            // `totalEnergy`). It therefore needs the same downward-step clamp as
            // the other lifetime gauges, or an upstream recompute would look
            // like a counter reset to `increase()`/`rate()`.
            crate::metrics::set_lifetime_monotonic(
                &metrics.energy_today.get_or_create(labels),
                v,
                "optimizer_energy_today",
            );
        }
        if let Some(v) = r.last_measurement {
            metrics.last_measurement.get_or_create(labels).set(v);
        }
    }

    let now = jiff::Timestamp::now().as_second() as f64;

    // Site-level battery charge/discharge (label-less gauges). `Some` here
    // always carries at least one value — `fetch_battery_energy` turns a
    // parsed-but-empty 200 (schema drift) into an error, logged above. Stamp
    // the dedicated `last_refresh` only on real data so the staleness alert
    // catches a drift instead of the gauges silently freezing.
    if let Some(be) = battery_energy.as_ref() {
        if let Some(v) = be.charged_watt_hours() {
            crate::metrics::set_lifetime_monotonic(
                &metrics.battery_energy_charged.get_or_create(&NoLabels {}),
                v,
                "battery_energy_charged",
            );
        }
        if let Some(v) = be.discharged_watt_hours() {
            crate::metrics::set_lifetime_monotonic(
                &metrics
                    .battery_energy_discharged
                    .get_or_create(&NoLabels {}),
                v,
                "battery_energy_discharged",
            );
        }
        metrics
            .last_refresh
            .get_or_create(&RefreshKind {
                kind: "battery_energy".into(),
            })
            .set(now);
    }

    if telemetry_committed {
        metrics
            .last_refresh
            .get_or_create(&RefreshKind {
                kind: "telemetry".into(),
            })
            .set(now);
    } else if !optimizers.is_empty() {
        // No optimizer yielded a usable measurement, yet the calls didn't all
        // error (those log individually). This is the silent-drift trip for the
        // `telemetry` staleness alert — make it obvious in the logs.
        warn!(
            optimizers = optimizers.len(),
            "no optimizer telemetry committed this cycle: all optimizers returned no usable \
             power/voltage/current. Optimizer metrics are now stale (the 'telemetry' staleness \
             alert will fire). The layout/information/optimizers response shape may have changed."
        );
    }
}

async fn fetch_live_with_metrics(
    client: &PortalClient,
    metrics: &AppMetrics,
    serials: &[String],
) -> Option<crate::portal::models::OptimizersInfoResponse> {
    match client.fetch_optimizers_live(serials).await {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(
                error = %e,
                optimizers = serials.len(),
                "fetch_optimizers_live failed; all optimizer telemetry will go stale this cycle"
            );
            metrics
                .refresh_errors
                .get_or_create(&RefreshKind {
                    kind: "optimizer".into(),
                })
                .inc();
            None
        }
    }
}

/// Per-optimizer lifetime energy, fanned out with bounded concurrency.
///
/// The energy-graph endpoint returns a single `totalEnergy` scalar for whatever
/// serials it is given, so attributing energy to an optimizer requires one call
/// each. Returns a serial → Wh map; missing/failed entries are simply absent and
/// leave the corresponding gauge at its previous value.
async fn fetch_energy_with_metrics(
    client: &Arc<PortalClient>,
    metrics: &AppMetrics,
    serials: &[String],
) -> HashMap<String, Option<f64>> {
    let mut out: HashMap<String, Option<f64>> = HashMap::with_capacity(serials.len());
    if serials.is_empty() {
        return out;
    }
    let start = Instant::now();
    let mut failures = 0usize;

    for chunk in serials.chunks(ENERGY_CONCURRENCY) {
        let mut set = tokio::task::JoinSet::new();
        for serial in chunk {
            let client = client.clone();
            let serial = serial.clone();
            set.spawn(async move {
                let result = client.fetch_optimizer_energy(&serial).await;
                (serial, result)
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((serial, Ok(wh))) => {
                    out.insert(serial, wh);
                }
                Ok((serial, Err(e))) => {
                    failures += 1;
                    warn!(
                        optimizer = %serial,
                        error = %e,
                        "fetch_optimizer_energy failed; leaving its energy gauge unchanged"
                    );
                }
                Err(e) => {
                    failures += 1;
                    warn!(error = %e, "energy fetch task panicked");
                }
            }
        }
    }

    if failures > 0 {
        metrics
            .refresh_errors
            .get_or_create(&RefreshKind {
                kind: "energy".into(),
            })
            .inc_by(failures as u64);
    }
    // Stamp `energy` freshness only when at least one optimizer reported, so a
    // total outage trips the staleness alert instead of silently freezing.
    if out.values().any(Option::is_some) {
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
    }
    out
}

fn make_labels(opt: &FlatOptimizer, config: &Config) -> OptimizerLabels {
    OptimizerLabels {
        optimizer: opt.serial_number.clone(),
        display_name: opt.display_name.clone(),
        inverter: opt.inverter_serial.clone(),
        field: config.field_for(&opt.serial_number).to_string(),
    }
}

/// Parse the portal's last-measurement timestamp.
///
/// Two formats, tried in order:
///
/// 1. **ISO 8601** — what the ONE `/services/layout/information/optimizers`
///    endpoint emits (`2026-07-28T10:15:00Z`, or with a numeric offset, or with
///    no offset at all). An offsetless value is interpreted in the system TZ,
///    same as the legacy branch.
/// 2. **Legacy** `"Thu Apr 23 12:26:12 GMT 2026"` — the retired `systemData`
///    format. Kept because these endpoints are undocumented and may still
///    surface it. The TZ abbreviation is **discarded**: in practice the portal
///    labelled the string `GMT` but emitted the site's local wall-clock time
///    (verified 2026-04 against a Europe/Berlin site, where GMT timestamps were
///    ~2 h in the future relative to the fetch). The Python reference also drops
///    the TZ and parses naive. We interpret the naive datetime in the system TZ —
///    the Docker image sets this to Europe/Berlin, so metric timestamps align
///    with the rest of the log output.
fn parse_last_measurement(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(ts) = parse_iso_timestamp(s) {
        return Some(ts);
    }
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

/// Parse an ISO 8601 timestamp. Handles `Z`, a numeric offset, and an offsetless
/// local datetime (interpreted in the system TZ). Returns `None` for anything
/// that isn't ISO-shaped so the caller can fall through to the legacy format.
fn parse_iso_timestamp(s: &str) -> Option<i64> {
    if !s.contains('T') {
        return None;
    }
    // Offset-aware: `jiff::Timestamp` requires one and rejects the rest.
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(ts.as_second());
    }
    // Offsetless (e.g. `2026-07-28T10:15:00`) — attach the system TZ.
    let dt: jiff::civil::DateTime = s.parse().ok()?;
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
        assert!(parse_last_measurement("   ").is_none());
        assert!(parse_last_measurement("not a date").is_none());
        assert!(parse_last_measurement("Wed Foo 25 12:34:56 CET 2026").is_none());
        // ISO-ish but not parseable — must not be mistaken for a valid instant.
        assert!(parse_last_measurement("2026-13-45T99:99:99Z").is_none());
        assert!(parse_last_measurement("T").is_none());
    }

    /// The ONE `/services/layout/information/optimizers` endpoint emits ISO 8601
    /// where the retired `systemData` used `"Thu Apr 23 12:26:12 GMT 2026"`. The
    /// old parser required exactly 6 whitespace-separated parts, so every ISO
    /// value would have silently yielded `None` and dropped the
    /// `last_measurement` gauge.
    #[test]
    fn parse_last_measurement_accepts_iso_utc() {
        let ts = parse_last_measurement("2026-07-28T10:15:00Z").expect("ISO with Z");
        assert_eq!(ts, 1785233700);
    }

    #[test]
    fn parse_last_measurement_accepts_iso_with_offset() {
        // Same instant as the Z case, expressed as +02:00 local time.
        let ts = parse_last_measurement("2026-07-28T12:15:00+02:00").expect("ISO with offset");
        assert_eq!(ts, 1785233700);
    }

    #[test]
    fn parse_last_measurement_accepts_iso_with_fractional_seconds() {
        let ts = parse_last_measurement("2026-07-28T10:15:00.123Z").expect("ISO with millis");
        assert_eq!(ts, 1785233700);
    }

    #[test]
    fn parse_last_measurement_offsetless_iso_uses_system_tz() {
        // No offset ⇒ interpreted in the system TZ, matching how the legacy
        // branch treats its (misleadingly labelled) naive timestamps. Assert on
        // the round-tripped wall-clock fields rather than a fixed epoch, since
        // the test host's TZ is not fixed.
        let ts = parse_last_measurement("2026-07-28T10:15:00").expect("offsetless ISO");
        let back = jiff::Timestamp::from_second(ts)
            .expect("ts")
            .to_zoned(jiff::tz::TimeZone::system());
        assert_eq!(back.year(), 2026);
        assert_eq!(back.month(), 7);
        assert_eq!(back.day(), 28);
        assert_eq!(back.hour(), 10);
        assert_eq!(back.minute(), 15);
    }
}
