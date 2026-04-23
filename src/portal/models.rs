use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

/// Treat `null` as `T::default()` (serde's `default` attribute only covers
/// *missing* fields; the portal uses explicit `null` for optional children).
fn null_is_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutResponse {
    #[serde(rename = "logicalTree", default, deserialize_with = "null_is_default")]
    pub logical_tree: LayoutNode,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LayoutNode {
    #[serde(default, deserialize_with = "null_is_default")]
    pub data: NodeData,
    #[serde(default, deserialize_with = "null_is_default")]
    pub children: Vec<LayoutNode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeData {
    #[serde(default, deserialize_with = "null_is_default")]
    pub id: i64,
    #[serde(default, deserialize_with = "null_is_default")]
    pub name: String,
    #[serde(default, rename = "displayName", deserialize_with = "null_is_default")]
    pub display_name: String,
    #[serde(default, rename = "serialNumber", deserialize_with = "null_is_default")]
    pub serial_number: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptimizerData {
    #[serde(rename = "lastMeasurementDate", default)]
    pub last_measurement_date: String,
    #[serde(default)]
    pub measurements: HashMap<String, serde_json::Value>,
}

impl OptimizerData {
    pub fn current_amps(&self) -> Option<f64> {
        self.measurement("Current [A]")
    }
    pub fn voltage_volts(&self) -> Option<f64> {
        self.measurement("Voltage [V]")
    }
    pub fn power_watts(&self) -> Option<f64> {
        self.measurement("Power [W]")
    }
    pub fn optimizer_voltage_volts(&self) -> Option<f64> {
        self.measurement("Optimizer Voltage [V]")
    }

    fn measurement(&self, key: &str) -> Option<f64> {
        match self.measurements.get(key)? {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => parse_number(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptimizerEnergy {
    /// `None` when the field is absent, `null`, or unparseable. Accepts either
    /// a JSON number (current portal behaviour) or a JSON string (older
    /// responses the Python reference was built against).
    #[serde(rename = "unscaledEnergy", default, deserialize_with = "flexible_f64")]
    pub unscaled_energy: Option<f64>,
}

impl OptimizerEnergy {
    pub fn watt_hours(&self) -> Option<f64> {
        self.unscaled_energy
    }
}

fn flexible_f64<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => Ok(n.as_f64()),
        Some(serde_json::Value::String(s)) => Ok(parse_number(&s)),
        _ => Ok(None),
    }
}

/// Parse a numeric string tolerating both `1234.5` and `1234,5` decimal
/// separators. Safety net for when `locale=en_US` isn't honoured.
fn parse_number(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    s.replace(',', ".").parse::<f64>().ok()
}

pub type EnergyResponse = HashMap<String, OptimizerEnergy>;

#[derive(Debug, Clone)]
pub struct FlatOptimizer {
    pub reporter_id: i64,
    pub serial_number: String,
    pub display_name: String,
    pub inverter_serial: String,
    pub inverter_display_name: String,
}

pub fn flatten_layout(resp: &LayoutResponse) -> Vec<FlatOptimizer> {
    let mut out = Vec::new();
    for child in &resp.logical_tree.children {
        let upper = child.data.name.to_ascii_uppercase();
        if upper.contains("PRODUCTION METER") {
            for inverter in &child.children {
                collect_inverter(inverter, &mut out);
            }
        } else {
            collect_inverter(child, &mut out);
        }
    }
    out
}

fn collect_inverter(inverter: &LayoutNode, out: &mut Vec<FlatOptimizer>) {
    for child in &inverter.children {
        let upper = child.data.name.to_ascii_uppercase();
        if upper.contains("STRING") {
            for opt in &child.children {
                push_optimizer(inverter, opt, out);
            }
        } else {
            for string in &child.children {
                for opt in &string.children {
                    push_optimizer(inverter, opt, out);
                }
            }
        }
    }
}

fn push_optimizer(inverter: &LayoutNode, opt: &LayoutNode, out: &mut Vec<FlatOptimizer>) {
    out.push(FlatOptimizer {
        reporter_id: opt.data.id,
        serial_number: opt.data.serial_number.clone(),
        display_name: opt.data.display_name.clone(),
        inverter_serial: inverter.data.serial_number.clone(),
        inverter_display_name: inverter.data.display_name.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_simple_tree() {
        let json = r#"{
            "siteId": 42,
            "logicalTree": {
                "data": {"id": 1, "name": "Site", "type": "SITE"},
                "children": [{
                    "data": {"id": 2, "name": "Inverter 1", "displayName": "Inv1", "serialNumber": "INV1", "type": "INVERTER"},
                    "children": [{
                        "data": {"id": 3, "name": "String A", "type": "STRING"},
                        "children": [
                            {"data": {"id": 10, "name": "1.1.1", "displayName": "1.1.1", "serialNumber": "OPT1", "type": "OPTIMIZER"}, "children": []},
                            {"data": {"id": 11, "name": "1.1.2", "displayName": "1.1.2", "serialNumber": "OPT2", "type": "OPTIMIZER"}, "children": []}
                        ]
                    }]
                }]
            }
        }"#;
        let resp: LayoutResponse = serde_json::from_str(json).expect("valid layout fixture");
        let flat = flatten_layout(&resp);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].serial_number, "OPT1");
        assert_eq!(flat[0].inverter_serial, "INV1");
        assert_eq!(flat[1].serial_number, "OPT2");
    }

    #[test]
    fn measurements_parse_dot_and_comma_decimals() {
        let json = r#"{
            "lastMeasurementDate": "Thu Apr 23 12:26:12 GMT 2026",
            "measurements": {
                "Power [W]": "252.19",
                "Voltage [V]": "50,12",
                "Current [A]": 5.03,
                "Optimizer Voltage [V]": "58,5"
            }
        }"#;
        let data: OptimizerData = serde_json::from_str(json).expect("parse");
        assert_eq!(data.power_watts(), Some(252.19));
        assert_eq!(data.voltage_volts(), Some(50.12));
        assert_eq!(data.current_amps(), Some(5.03));
        assert_eq!(data.optimizer_voltage_volts(), Some(58.5));
    }

    #[test]
    fn energy_accepts_number_or_string() {
        let number: OptimizerEnergy =
            serde_json::from_str(r#"{"unscaledEnergy": 74138.0}"#).expect("number form");
        assert_eq!(number.watt_hours(), Some(74138.0));
        let string: OptimizerEnergy =
            serde_json::from_str(r#"{"unscaledEnergy": "74138.0"}"#).expect("string form");
        assert_eq!(string.watt_hours(), Some(74138.0));
        let null: OptimizerEnergy =
            serde_json::from_str(r#"{"unscaledEnergy": null}"#).expect("null form");
        assert_eq!(null.watt_hours(), None);
        let missing: OptimizerEnergy = serde_json::from_str(r#"{}"#).expect("missing form");
        assert_eq!(missing.watt_hours(), None);
    }

    #[test]
    fn parses_null_fields_from_portal() {
        // The real portal emits explicit `null` for missing optional fields
        // (e.g. `data: null` at the root of `logicalTree`, or a null
        // `serialNumber` on non-physical nodes). Plain `#[serde(default)]`
        // wouldn't cover this; the `null_is_default` helper does.
        let json = r#"{
            "logicalTree": {
                "data": null,
                "children": [{
                    "data": {"id": 1, "name": "Inverter", "serialNumber": null, "displayName": null, "type": "INVERTER"},
                    "children": null
                }]
            }
        }"#;
        let resp: LayoutResponse = serde_json::from_str(json).expect("null-tolerant parse");
        assert_eq!(resp.logical_tree.children.len(), 1);
        assert_eq!(resp.logical_tree.children[0].data.name, "Inverter");
        assert_eq!(resp.logical_tree.children[0].data.serial_number, "");
        assert!(resp.logical_tree.children[0].children.is_empty());
    }

    #[test]
    fn flatten_with_production_meter() {
        let json = r#"{
            "siteId": 42,
            "logicalTree": {
                "data": {"id": 1, "name": "Site"},
                "children": [{
                    "data": {"id": 99, "name": "Production Meter", "type": "METER"},
                    "children": [{
                        "data": {"id": 2, "name": "Inverter 1", "serialNumber": "INV1", "type": "INVERTER"},
                        "children": [{
                            "data": {"id": 3, "name": "String A", "type": "STRING"},
                            "children": [
                                {"data": {"id": 10, "serialNumber": "OPT1", "type": "OPTIMIZER"}, "children": []}
                            ]
                        }]
                    }]
                }]
            }
        }"#;
        let resp: LayoutResponse = serde_json::from_str(json).expect("valid layout fixture");
        let flat = flatten_layout(&resp);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].serial_number, "OPT1");
        assert_eq!(flat[0].inverter_serial, "INV1");
    }

    #[test]
    fn optimizer_measurements() {
        let json = r#"{
            "lastMeasurementDate": "Wed Sep 25 12:34:56 CEST 2026",
            "measurements": {
                "Current [A]": 2.5,
                "Voltage [V]": 38.2,
                "Power [W]": 95.5,
                "Optimizer Voltage [V]": 380.0
            }
        }"#;
        let data: OptimizerData = serde_json::from_str(json).expect("valid optimizer fixture");
        assert_eq!(data.current_amps(), Some(2.5));
        assert_eq!(data.power_watts(), Some(95.5));
        assert_eq!(data.voltage_volts(), Some(38.2));
        assert_eq!(data.optimizer_voltage_volts(), Some(380.0));
    }
}
