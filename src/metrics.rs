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
