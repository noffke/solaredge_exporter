use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OptimizerLabels {
    pub optimizer: String,
    pub display_name: String,
    pub inverter: String,
    pub field: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RefreshKind {
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BatteryLabels {
    pub battery: String,
    pub model: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MeterLabels {
    pub meter: String,
    pub inverter: String,
    // Rust keyword → raw identifier. prometheus-client's EncodeLabelSet emits
    // it as the bare label name `type` in the OpenMetrics output.
    pub r#type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MonitoringEndpoint {
    pub endpoint: String,
}

/// Label set for a single, label-less site-level series. It exists only so the
/// metric can be modeled as a `Family`, which (unlike a plain `Gauge`) renders
/// *nothing* until a sample is created — see the comment on the lifetime energy
/// fields below. Must be an empty *named* struct: the `EncodeLabelSet` derive
/// rejects unit structs (`struct Foo;`) but accepts `struct Foo {}` and emits
/// no labels for it.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct NoLabels {}

pub struct AppMetrics {
    registry: Registry,

    pub power: Family<OptimizerLabels, Gauge<f64, AtomicU64>>,
    pub module_voltage: Family<OptimizerLabels, Gauge<f64, AtomicU64>>,
    pub dc_voltage: Family<OptimizerLabels, Gauge<f64, AtomicU64>>,
    pub current: Family<OptimizerLabels, Gauge<f64, AtomicU64>>,
    pub energy_today: Family<OptimizerLabels, Gauge<f64, AtomicU64>>,
    pub last_measurement: Family<OptimizerLabels, Gauge<f64, AtomicU64>>,

    pub last_refresh: Family<RefreshKind, Gauge<f64, AtomicU64>>,
    pub refresh_duration: Family<RefreshKind, Gauge<f64, AtomicU64>>,
    pub refresh_errors: Family<RefreshKind, Counter>,
    pub login_count: Counter,

    // Site-level battery charge/discharge energy (from the portal dashboard
    // energy endpoint — the public storageData API reports these as 0 for
    // SolarEdge Home Battery 48V).
    //
    // These are label-less *families*, not plain gauges, so that they are
    // ABSENT (emit no sample) until the first successful portal fetch — a plain
    // `Gauge` defaults to 0 and would be scraped at 0 between process start and
    // that first fetch. A 0 sample on a lifetime total is poison for
    // `increase()`/`rate()`: PromQL's counter-reset compensation reads the
    // restart transition `350k -> 0 -> 351k` as a reset and adds the full
    // pre-reset ~350k to the window, fabricating a day's worth of impossible
    // charge/discharge. Absence has no such downward step. (Renaming to a
    // `_total` counter does NOT help — the same compensation applies.)
    pub battery_energy_charged: Family<NoLabels, Gauge<f64, AtomicU64>>,
    pub battery_energy_discharged: Family<NoLabels, Gauge<f64, AtomicU64>>,

    // Battery (from /site/{id}/storageData)
    pub battery_ac_grid_charging: Family<BatteryLabels, Counter<f64, AtomicU64>>,
    pub battery_full_pack_energy: Family<BatteryLabels, Gauge<f64, AtomicU64>>,
    pub battery_power: Family<BatteryLabels, Gauge<f64, AtomicU64>>,
    pub battery_internal_temp: Family<BatteryLabels, Gauge<f64, AtomicU64>>,
    pub battery_state: Family<BatteryLabels, Gauge<f64, AtomicU64>>,

    // Site-level meter lifetime counters (from /site/{id}/meters)
    pub monitoring_meter_lifetime_energy: Family<MeterLabels, Gauge<f64, AtomicU64>>,

    // Site PV lifetime (from /site/{id}/overview). Label-less family for the
    // same absent-until-first-value reason as the battery energy fields above.
    pub site_pv_lifetime_energy: Family<NoLabels, Gauge<f64, AtomicU64>>,

    // Public Monitoring API operational metrics
    pub monitoring_api_last_refresh: Family<MonitoringEndpoint, Gauge<f64, AtomicU64>>,
    pub monitoring_api_refresh_duration: Family<MonitoringEndpoint, Gauge<f64, AtomicU64>>,
    pub monitoring_api_refresh_errors: Family<MonitoringEndpoint, Counter>,
    pub monitoring_api_requests: Family<MonitoringEndpoint, Counter>,
}

impl AppMetrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("solaredge");

        let power: Family<OptimizerLabels, Gauge<f64, AtomicU64>> = Family::default();
        let module_voltage: Family<OptimizerLabels, Gauge<f64, AtomicU64>> = Family::default();
        let dc_voltage: Family<OptimizerLabels, Gauge<f64, AtomicU64>> = Family::default();
        let current: Family<OptimizerLabels, Gauge<f64, AtomicU64>> = Family::default();
        let energy_today: Family<OptimizerLabels, Gauge<f64, AtomicU64>> = Family::default();
        let last_measurement: Family<OptimizerLabels, Gauge<f64, AtomicU64>> = Family::default();

        let last_refresh: Family<RefreshKind, Gauge<f64, AtomicU64>> = Family::default();
        let refresh_duration: Family<RefreshKind, Gauge<f64, AtomicU64>> = Family::default();
        let refresh_errors: Family<RefreshKind, Counter> = Family::default();
        let login_count = Counter::default();

        let battery_energy_charged: Family<NoLabels, Gauge<f64, AtomicU64>> = Family::default();
        let battery_energy_discharged: Family<NoLabels, Gauge<f64, AtomicU64>> = Family::default();
        let battery_ac_grid_charging: Family<BatteryLabels, Counter<f64, AtomicU64>> =
            Family::default();
        let battery_full_pack_energy: Family<BatteryLabels, Gauge<f64, AtomicU64>> =
            Family::default();
        let battery_power: Family<BatteryLabels, Gauge<f64, AtomicU64>> = Family::default();
        let battery_internal_temp: Family<BatteryLabels, Gauge<f64, AtomicU64>> = Family::default();
        let battery_state: Family<BatteryLabels, Gauge<f64, AtomicU64>> = Family::default();
        let monitoring_meter_lifetime_energy: Family<MeterLabels, Gauge<f64, AtomicU64>> =
            Family::default();
        let site_pv_lifetime_energy: Family<NoLabels, Gauge<f64, AtomicU64>> = Family::default();
        let monitoring_api_last_refresh: Family<MonitoringEndpoint, Gauge<f64, AtomicU64>> =
            Family::default();
        let monitoring_api_refresh_duration: Family<MonitoringEndpoint, Gauge<f64, AtomicU64>> =
            Family::default();
        let monitoring_api_refresh_errors: Family<MonitoringEndpoint, Counter> = Family::default();
        let monitoring_api_requests: Family<MonitoringEndpoint, Counter> = Family::default();

        registry.register(
            "optimizer_power_watts",
            "Instantaneous AC power reported by the optimizer",
            power.clone(),
        );
        registry.register(
            "optimizer_module_voltage_volts",
            "Voltage at the PV module terminals",
            module_voltage.clone(),
        );
        registry.register(
            "optimizer_dc_voltage_volts",
            "DC voltage at the optimizer output",
            dc_voltage.clone(),
        );
        registry.register(
            "optimizer_current_amperes",
            "DC current through the optimizer",
            current.clone(),
        );
        registry.register(
            "optimizer_energy_today_watt_hours",
            "Energy produced by the optimizer since the start of the current day, in Wh",
            energy_today.clone(),
        );
        registry.register(
            "optimizer_last_measurement_timestamp_seconds",
            "Unix timestamp of the optimizer's most recent measurement",
            last_measurement.clone(),
        );

        registry.register(
            "portal_last_refresh_timestamp_seconds",
            "Unix timestamp of the last successful portal refresh",
            last_refresh.clone(),
        );
        registry.register(
            "portal_refresh_duration_seconds",
            "Wall-clock duration of the last portal refresh",
            refresh_duration.clone(),
        );
        registry.register(
            "portal_refresh_errors",
            "Count of failed portal refresh attempts",
            refresh_errors.clone(),
        );
        registry.register(
            "portal_login",
            "Count of SolarEdge portal (re)logins",
            login_count.clone(),
        );

        registry.register(
            "battery_energy_charged_watt_hours",
            "Site lifetime energy charged into the battery from PV, in Wh (from the portal dashboard energy endpoint; excludes grid charging — see battery_ac_grid_charging_watt_hours)",
            battery_energy_charged.clone(),
        );
        registry.register(
            "battery_energy_discharged_watt_hours",
            "Site lifetime energy discharged from the battery to the home, in Wh (from the portal dashboard energy endpoint)",
            battery_energy_discharged.clone(),
        );
        registry.register(
            "battery_ac_grid_charging_watt_hours",
            "Cumulative AC energy used to charge the battery from the grid (Wh, monotonic counter). Accumulated from non-overlapping API query windows and persisted to monitoring_api.state_file across exporter restarts",
            battery_ac_grid_charging.clone(),
        );
        registry.register(
            "battery_full_pack_energy_watt_hours",
            "Current maximum energy storable in the battery, in Wh (divide by nameplate for State-of-Health)",
            battery_full_pack_energy.clone(),
        );
        registry.register(
            "battery_power_watts",
            "Battery instantaneous power (positive = charging, negative = discharging)",
            battery_power.clone(),
        );
        registry.register(
            "battery_internal_temperature_celsius",
            "Battery internal temperature",
            battery_internal_temp.clone(),
        );
        registry.register(
            "battery_state",
            "Raw batteryState value reported by the API. The public docs list 0=Invalid/1=Standby/2=ThermalMgmt/3=Enabled/4=Fault, but that mapping is stale for newer SolarEdge Home Battery families (value 4 has been observed on healthy discharging batteries) — interpret in conjunction with the portal UI",
            battery_state.clone(),
        );

        registry.register(
            "monitoring_meter_lifetime_energy_watt_hours",
            "Lifetime energy reading from a site meter (Production/Consumption/FeedIn/Purchased)",
            monitoring_meter_lifetime_energy.clone(),
        );

        registry.register(
            "site_pv_lifetime_energy_watt_hours",
            "Lifetime PV energy produced at the site, in Wh",
            site_pv_lifetime_energy.clone(),
        );

        registry.register(
            "monitoring_api_last_refresh_timestamp_seconds",
            "Unix timestamp of the last successful public Monitoring API call per endpoint",
            monitoring_api_last_refresh.clone(),
        );
        registry.register(
            "monitoring_api_refresh_duration_seconds",
            "Wall-clock duration of the last successful public Monitoring API call per endpoint",
            monitoring_api_refresh_duration.clone(),
        );
        registry.register(
            "monitoring_api_refresh_errors",
            "Count of failed public Monitoring API calls per endpoint",
            monitoring_api_refresh_errors.clone(),
        );
        registry.register(
            "monitoring_api_requests",
            "Count of public Monitoring API calls per endpoint (watch against the 300/day cap)",
            monitoring_api_requests.clone(),
        );

        Self {
            registry,
            power,
            module_voltage,
            dc_voltage,
            current,
            energy_today,
            last_measurement,
            last_refresh,
            refresh_duration,
            refresh_errors,
            login_count,
            battery_energy_charged,
            battery_energy_discharged,
            battery_ac_grid_charging,
            battery_full_pack_energy,
            battery_power,
            battery_internal_temp,
            battery_state,
            monitoring_meter_lifetime_energy,
            site_pv_lifetime_energy,
            monitoring_api_last_refresh,
            monitoring_api_refresh_duration,
            monitoring_api_refresh_errors,
            monitoring_api_requests,
        }
    }

    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut buf = String::new();
        encode(&mut buf, &self.registry)?;
        Ok(buf)
    }
}

impl Default for AppMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_contains_metric_names() {
        let m = AppMetrics::new();
        // Families without samples don't render in OpenMetrics — seed one sample
        // per family so the TYPE/HELP lines and metric names appear.
        let labels = OptimizerLabels {
            optimizer: "x".into(),
            display_name: "x".into(),
            inverter: "x".into(),
            field: "unassigned".into(),
        };
        m.power.get_or_create(&labels).set(0.0);
        m.login_count.inc();
        let out = m.encode().expect("encode");
        assert!(
            out.contains("solaredge_optimizer_power_watts"),
            "actual output:\n{out}"
        );
        assert!(
            out.contains("solaredge_portal_login_total"),
            "actual output:\n{out}"
        );
    }

    #[test]
    fn lifetime_gauges_absent_until_set() {
        // Regression guard for the restart-to-0 phantom-increase bug: the
        // lifetime energy series must emit NO sample (not 0) until the first
        // real value, so `increase()`/`rate()` never see a 0-dip on restart.
        let m = AppMetrics::new();
        let before = m.encode().expect("encode");
        assert!(
            !before.contains("solaredge_battery_energy_charged_watt_hours 0"),
            "lifetime gauge must not emit a 0 sample before first set:\n{before}"
        );
        // No sample line at all (HELP/TYPE may still be absent for empty families).
        assert!(
            !before.lines().any(
                |l| l.starts_with("solaredge_battery_energy_charged_watt_hours ")
                    || l.starts_with("solaredge_battery_energy_charged_watt_hours{")
            ),
            "lifetime gauge must be absent before first set:\n{before}"
        );

        m.battery_energy_charged
            .get_or_create(&NoLabels {})
            .set(123.0);
        let after = m.encode().expect("encode");
        assert!(
            after.contains("solaredge_battery_energy_charged_watt_hours"),
            "metric name must appear after set:\n{after}"
        );
        assert!(
            after.contains("123"),
            "value must appear after set:\n{after}"
        );
    }

    #[test]
    fn gauge_values_round_trip() {
        let m = AppMetrics::new();
        let labels = OptimizerLabels {
            optimizer: "OPT1".into(),
            display_name: "1.1.1".into(),
            inverter: "INV1".into(),
            field: "east".into(),
        };
        m.power.get_or_create(&labels).set(123.4);
        let out = m.encode().expect("encode");
        assert!(out.contains("123.4"));
        assert!(out.contains("optimizer=\"OPT1\""));
        assert!(out.contains("field=\"east\""));
    }
}
