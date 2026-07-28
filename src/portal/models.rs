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

/// One node of the ONE platform's logical site structure
/// (`GET /services/layout/logical/generic/v2/site/{id}?include-optimizers=true`).
///
/// Unlike the retired `layout/logical` tree — which carried a `data` sub-object
/// and forced us to identify levels by substring-matching `name` — every v2 node
/// is self-describing via `type` (`INVERTER`, `STRING`, `OPTIMIZER`, or a
/// `FOLDER` that merely groups same-typed siblings).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LayoutNodeV2 {
    #[serde(default, deserialize_with = "null_is_default")]
    pub uuid: String,
    #[serde(default, deserialize_with = "null_is_default")]
    pub serial: String,
    #[serde(default, deserialize_with = "null_is_default")]
    pub name: String,
    #[serde(rename = "type", default, deserialize_with = "null_is_default")]
    pub node_type: String,
    #[serde(default, deserialize_with = "null_is_default")]
    pub properties: NodeProperties,
    #[serde(default, deserialize_with = "null_is_default")]
    pub children: Vec<LayoutNodeV2>,
    /// Set when the API wraps the tree in a `siteStructure` envelope. The
    /// reference implementation accepts both shapes, so we do too.
    #[serde(
        rename = "siteStructure",
        default,
        deserialize_with = "null_is_default"
    )]
    pub site_structure: Option<Box<LayoutNodeV2>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeProperties {
    #[serde(default, deserialize_with = "null_is_default")]
    pub identifier: String,
    #[serde(default, deserialize_with = "null_is_default")]
    pub status: String,
}

impl LayoutNodeV2 {
    /// The actual tree root, unwrapping the optional `siteStructure` envelope.
    pub fn root(&self) -> &LayoutNodeV2 {
        self.site_structure.as_deref().unwrap_or(self)
    }

    fn is_type(&self, want: &str) -> bool {
        self.node_type.eq_ignore_ascii_case(want)
    }

    /// Children of type `want`, transparently descending through `FOLDER`
    /// grouping nodes. The v2 tree wraps each device level in a folder
    /// (`FOLDER("STRING") > STRING`), but we accept the direct shape too so a
    /// site laid out without folders — or a future flattening upstream — still
    /// resolves.
    fn typed_children(&self, want: &str) -> Vec<&LayoutNodeV2> {
        let mut out = Vec::new();
        for child in &self.children {
            if child.is_type(want) {
                out.push(child);
            } else if child.is_type("FOLDER") {
                for grandchild in &child.children {
                    if grandchild.is_type(want) {
                        out.push(grandchild);
                    }
                }
            }
        }
        out
    }

    /// Device serial, following the reference implementation's precedence:
    /// explicit `serial`, then `properties.identifier`, then `uuid`.
    fn best_serial(&self) -> String {
        for candidate in [&self.serial, &self.properties.identifier, &self.uuid] {
            if !candidate.trim().is_empty() {
                return candidate.clone();
            }
        }
        String::new()
    }
}

/// Response of `POST /services/layout/information/optimizers` (body: a JSON
/// array of optimizer serials). One call covers every optimizer on the site,
/// replacing the retired one-GET-per-optimizer `systemData` polling.
///
/// The response also carries a `basicInformationList` (per-optimizer model,
/// panel description, azimuth/tilt). We don't model it — none of it feeds a
/// metric today, and declaring fields we never read is just maintenance debt.
/// Add it back alongside the metric that needs it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OptimizersInfoResponse {
    /// Keyed by optimizer serial. An optimizer present in the layout but
    /// inactive/replaced has no entry here.
    #[serde(
        rename = "serialToLiveData",
        default,
        deserialize_with = "null_is_default"
    )]
    pub serial_to_live_data: HashMap<String, LiveData>,
}

/// Live electrical telemetry for one optimizer. Field names map 1:1 onto the
/// gauges the retired `systemData` `measurements` map used to feed:
/// `power_W` → `Power [W]`, `voltage_V` → `Voltage [V]`,
/// `optimizerVoltage_V` → `Optimizer Voltage [V]`, `current_A` → `Current [A]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LiveData {
    #[serde(rename = "power_W", default, deserialize_with = "flexible_f64")]
    pub power_w: Option<f64>,
    #[serde(rename = "current_A", default, deserialize_with = "flexible_f64")]
    pub current_a: Option<f64>,
    #[serde(rename = "voltage_V", default, deserialize_with = "flexible_f64")]
    pub voltage_v: Option<f64>,
    #[serde(
        rename = "optimizerVoltage_V",
        default,
        deserialize_with = "flexible_f64"
    )]
    pub optimizer_voltage_v: Option<f64>,
    /// ISO 8601 on this API (the retired endpoint emitted
    /// `"Thu Apr 23 12:26:12 GMT 2026"`). Empty for inactive optimizers.
    #[serde(
        rename = "lastMeasurement",
        default,
        deserialize_with = "null_is_default"
    )]
    pub last_measurement: String,
}

impl LiveData {
    /// True when the optimizer reported at least one electrical value. Used to
    /// distinguish "inactive/no data" from "schema drifted".
    pub fn has_measurements(&self) -> bool {
        self.power_w.is_some()
            || self.current_a.is_some()
            || self.voltage_v.is_some()
            || self.optimizer_voltage_v.is_some()
    }
}

/// Response of `GET /services/layout/energy-graph/site/{id}/optimizers`.
///
/// `totalEnergy` is a **single scalar covering all requested serials**, so this
/// endpoint has to be queried one optimizer at a time to attribute energy —
/// batching would silently sum the site. Lifetime Wh.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EnergyGraphResponse {
    #[serde(rename = "totalEnergy", default, deserialize_with = "flexible_f64")]
    pub total_energy: Option<f64>,
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

#[derive(Debug, Clone)]
pub struct FlatOptimizer {
    pub serial_number: String,
    pub display_name: String,
    pub inverter_serial: String,
    pub inverter_display_name: String,
    /// `properties.status` from the layout (e.g. `ACTIVE`, `REPLACED`). Replaced
    /// optimizers stay in the layout indefinitely and never report live data, so
    /// this lets the scrape loop log them at DEBUG instead of warning forever.
    pub status: String,
}

impl FlatOptimizer {
    /// True unless the layout explicitly marks the optimizer as not active.
    /// Unknown/empty status counts as active — we only want to silence units
    /// SolarEdge has positively told us are gone.
    pub fn is_active(&self) -> bool {
        let s = self.status.trim();
        s.is_empty() || s.eq_ignore_ascii_case("ACTIVE")
    }
}

/// Flatten the v2 logical tree to the optimizer list the scrape loop iterates.
///
/// Shape (each device level wrapped in a grouping `FOLDER`):
/// `FOLDER(INVERTER) > INVERTER > FOLDER(STRING) > STRING > FOLDER(OPTIMIZER) > OPTIMIZER`.
/// `typed_children` descends through the folders, and optimizers hanging
/// directly off an inverter (no string level) are still collected.
pub fn flatten_layout_v2(resp: &LayoutNodeV2) -> Vec<FlatOptimizer> {
    let root = resp.root();
    let mut out = Vec::new();
    for inverter in root.typed_children("INVERTER") {
        let inverter_serial = inverter.best_serial();
        let inverter_display_name = if inverter.name.trim().is_empty() {
            inverter_serial.clone()
        } else {
            inverter.name.clone()
        };

        let strings = inverter.typed_children("STRING");
        if strings.is_empty() {
            // Tolerate a site with no string level.
            for opt in inverter.typed_children("OPTIMIZER") {
                out.push(flat_optimizer(
                    opt,
                    &inverter_serial,
                    &inverter_display_name,
                ));
            }
            continue;
        }
        for string in strings {
            for opt in string.typed_children("OPTIMIZER") {
                out.push(flat_optimizer(
                    opt,
                    &inverter_serial,
                    &inverter_display_name,
                ));
            }
        }
    }
    out
}

fn flat_optimizer(
    opt: &LayoutNodeV2,
    inverter_serial: &str,
    inverter_display_name: &str,
) -> FlatOptimizer {
    let serial_number = opt.best_serial();
    // The retired tree's `displayName` for an optimizer was the panel name
    // ("1.1.1"), which is v2's `name` — keep using it so the `display_name`
    // metric label doesn't churn.
    let display_name = if opt.name.trim().is_empty() {
        serial_number.clone()
    } else {
        opt.name.clone()
    };
    FlatOptimizer {
        serial_number,
        display_name,
        inverter_serial: inverter_serial.to_string(),
        inverter_display_name: inverter_display_name.to_string(),
        status: opt.properties.status.clone(),
    }
}

/// Response of `GET /services/dashboard/energy/sites/{id}`. We only model the
/// `summary` block — `summary.productionDistribution.productionToBattery` is the
/// energy charged into the battery from PV, and
/// `summary.consumptionDistribution.consumptionFromBattery` is the energy
/// discharged from the battery to the home. Both are cumulative over the queried
/// window. These plug the gap left by the public storageData API, which reports
/// `lifeTimeEnergyCharged`/`Discharged` as 0 for the SolarEdge Home Battery 48V.
#[derive(Debug, Clone, Deserialize)]
pub struct DashboardEnergyResponse {
    #[serde(default, deserialize_with = "null_is_default")]
    pub summary: EnergySummary,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EnergySummary {
    #[serde(
        rename = "productionDistribution",
        default,
        deserialize_with = "null_is_default"
    )]
    pub production_distribution: ProductionDistribution,
    #[serde(
        rename = "consumptionDistribution",
        default,
        deserialize_with = "null_is_default"
    )]
    pub consumption_distribution: ConsumptionDistribution,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProductionDistribution {
    #[serde(
        rename = "productionToBattery",
        default,
        deserialize_with = "flexible_f64"
    )]
    pub production_to_battery: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConsumptionDistribution {
    #[serde(
        rename = "consumptionFromBattery",
        default,
        deserialize_with = "flexible_f64"
    )]
    pub consumption_from_battery: Option<f64>,
}

impl DashboardEnergyResponse {
    /// Cumulative Wh charged into the battery from PV over the queried window.
    pub fn charged_watt_hours(&self) -> Option<f64> {
        self.summary.production_distribution.production_to_battery
    }

    /// Cumulative Wh discharged from the battery to the home over the queried window.
    pub fn discharged_watt_hours(&self) -> Option<f64> {
        self.summary
            .consumption_distribution
            .consumption_from_battery
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full folder-wrapped v2 shape:
    /// `FOLDER(INVERTER) > INVERTER > FOLDER(STRING) > STRING > FOLDER(OPTIMIZER) > OPTIMIZER`.
    const V2_TREE: &str = r#"{
        "siteStructure": {
            "uuid": "site-uuid",
            "type": "SITE",
            "children": [{
                "type": "FOLDER",
                "name": "INVERTER",
                "children": [{
                    "type": "INVERTER",
                    "serial": "INV1",
                    "name": "Inverter 1",
                    "properties": {"identifier": "INV1-ident", "status": "ACTIVE"},
                    "children": [{
                        "type": "FOLDER",
                        "name": "STRING",
                        "children": [{
                            "type": "STRING",
                            "name": "String A",
                            "properties": {"identifier": "str-1", "status": "ACTIVE"},
                            "children": [{
                                "type": "FOLDER",
                                "name": "OPTIMIZER",
                                "children": [
                                    {"type": "OPTIMIZER", "serial": "OPT1", "name": "1.1.1",
                                     "uuid": "u1", "properties": {"status": "ACTIVE"}},
                                    {"type": "OPTIMIZER", "serial": "OPT2", "name": "1.1.2",
                                     "uuid": "u2", "properties": {"status": "REPLACED"}}
                                ]
                            }]
                        }]
                    }]
                }]
            }]
        }
    }"#;

    #[test]
    fn flatten_v2_folder_wrapped_tree() {
        let resp: LayoutNodeV2 = serde_json::from_str(V2_TREE).expect("valid v2 layout fixture");
        let flat = flatten_layout_v2(&resp);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].serial_number, "OPT1");
        assert_eq!(flat[0].display_name, "1.1.1");
        assert_eq!(flat[0].inverter_serial, "INV1");
        assert_eq!(flat[0].inverter_display_name, "Inverter 1");
        assert!(flat[0].is_active());
        assert_eq!(flat[1].serial_number, "OPT2");
        // A REPLACED optimizer is still listed — it just must not warn.
        assert!(!flat[1].is_active());
    }

    #[test]
    fn flatten_v2_without_envelope_or_folders() {
        // No `siteStructure` wrapper, and devices hang directly off their
        // parent with no grouping FOLDER. Both tolerances in one fixture.
        let json = r#"{
            "type": "SITE",
            "children": [{
                "type": "INVERTER",
                "serial": "INV9",
                "name": "Inv 9",
                "children": [{
                    "type": "STRING",
                    "name": "S1",
                    "children": [
                        {"type": "OPTIMIZER", "serial": "OPTX", "name": "9.1.1"}
                    ]
                }]
            }]
        }"#;
        let resp: LayoutNodeV2 = serde_json::from_str(json).expect("bare v2 fixture");
        let flat = flatten_layout_v2(&resp);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].serial_number, "OPTX");
        assert_eq!(flat[0].inverter_serial, "INV9");
    }

    #[test]
    fn flatten_v2_optimizers_without_string_level() {
        let json = r#"{
            "type": "SITE",
            "children": [{
                "type": "INVERTER", "serial": "INV1",
                "children": [{
                    "type": "FOLDER", "name": "OPTIMIZER",
                    "children": [{"type": "OPTIMIZER", "serial": "OPT1"}]
                }]
            }]
        }"#;
        let resp: LayoutNodeV2 = serde_json::from_str(json).expect("stringless fixture");
        let flat = flatten_layout_v2(&resp);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].serial_number, "OPT1");
        // No `name` on the node ⇒ display_name falls back to the serial.
        assert_eq!(flat[0].display_name, "OPT1");
    }

    #[test]
    fn flatten_v2_tolerates_nulls_and_serial_fallbacks() {
        // Explicit `null`s (which plain `#[serde(default)]` would reject) plus
        // an optimizer whose serial has to come from properties.identifier and
        // an inverter whose serial has to come from uuid.
        let json = r#"{
            "type": "SITE",
            "children": [{
                "type": "INVERTER",
                "serial": null,
                "name": null,
                "uuid": "inv-uuid",
                "properties": null,
                "children": [{
                    "type": "STRING",
                    "children": [{
                        "type": "OPTIMIZER",
                        "serial": null,
                        "properties": {"identifier": "OPT-IDENT", "status": null}
                    }]
                }]
            }]
        }"#;
        let resp: LayoutNodeV2 = serde_json::from_str(json).expect("null-tolerant parse");
        let flat = flatten_layout_v2(&resp);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].serial_number, "OPT-IDENT");
        assert_eq!(flat[0].inverter_serial, "inv-uuid");
        assert_eq!(flat[0].inverter_display_name, "inv-uuid");
        assert!(flat[0].is_active());
    }

    #[test]
    fn optimizers_info_maps_live_data() {
        let json = r#"{
            "basicInformationList": [
                {"serial": "OPT1", "model": "P370", "description": "SunPower SPR-MAX3-400"},
                {"serial": "OPT2", "model": "P370", "description": null}
            ],
            "serialToLiveData": {
                "OPT1": {
                    "power_W": 252.19,
                    "current_A": 5.03,
                    "voltage_V": "50,12",
                    "optimizerVoltage_V": 58.5,
                    "lastMeasurement": "2026-07-28T10:15:00Z"
                }
            }
        }"#;
        let r: OptimizersInfoResponse = serde_json::from_str(json).expect("optimizers info");

        let live = r.serial_to_live_data.get("OPT1").expect("OPT1 live data");
        assert_eq!(live.power_w, Some(252.19));
        assert_eq!(live.current_a, Some(5.03));
        // Comma decimals still tolerated via `flexible_f64`.
        assert_eq!(live.voltage_v, Some(50.12));
        assert_eq!(live.optimizer_voltage_v, Some(58.5));
        assert_eq!(live.last_measurement, "2026-07-28T10:15:00Z");
        assert!(live.has_measurements());

        // OPT2 is in the inventory but has no live entry (inactive/replaced).
        assert!(!r.serial_to_live_data.contains_key("OPT2"));
    }

    #[test]
    fn optimizers_info_tolerates_empty_and_null_blocks() {
        let r: OptimizersInfoResponse =
            serde_json::from_str(r#"{"basicInformationList": null, "serialToLiveData": null}"#)
                .expect("null blocks");
        assert!(r.serial_to_live_data.is_empty());

        let empty: OptimizersInfoResponse = serde_json::from_str("{}").expect("empty object");
        assert!(empty.serial_to_live_data.is_empty());

        // A live entry with no electrical values must not count as measured.
        let quiet: LiveData =
            serde_json::from_str(r#"{"lastMeasurement": ""}"#).expect("quiet optimizer");
        assert!(!quiet.has_measurements());
    }

    #[test]
    fn energy_graph_accepts_number_or_string() {
        let number: EnergyGraphResponse =
            serde_json::from_str(r#"{"totalEnergy": 74138.0}"#).expect("number form");
        assert_eq!(number.total_energy, Some(74138.0));
        let string: EnergyGraphResponse =
            serde_json::from_str(r#"{"totalEnergy": "74138.0"}"#).expect("string form");
        assert_eq!(string.total_energy, Some(74138.0));
        let null: EnergyGraphResponse =
            serde_json::from_str(r#"{"totalEnergy": null}"#).expect("null form");
        assert_eq!(null.total_energy, None);
        let missing: EnergyGraphResponse = serde_json::from_str("{}").expect("missing form");
        assert_eq!(missing.total_energy, None);
    }

    #[test]
    fn dashboard_energy_extracts_battery_charge_discharge() {
        // Trimmed real capture from
        // GET /services/dashboard/energy/sites/{id}. `productionToBattery` is
        // energy charged from PV; `consumptionFromBattery` is energy discharged
        // to the home.
        let json = r#"{
            "summary": {
                "production": 691862.9,
                "productionDistribution": {
                    "productionToHome": 423785.06,
                    "productionToBattery": 261387.97,
                    "productionToGrid": 6689.8296
                },
                "consumption": 727944.3,
                "consumptionDistribution": {
                    "consumptionFromBattery": 247399.02,
                    "consumptionFromSolar": 423785.06,
                    "consumptionFromGrid": 56760.246
                },
                "averagePowerFactor": null
            },
            "chart": {"measurements": []}
        }"#;
        let r: DashboardEnergyResponse = serde_json::from_str(json).expect("dashboard energy");
        assert_eq!(r.charged_watt_hours(), Some(261387.97));
        assert_eq!(r.discharged_watt_hours(), Some(247399.02));
    }

    #[test]
    fn dashboard_energy_tolerates_missing_storage_fields() {
        // A site without a battery omits the *ToBattery / *FromBattery keys
        // (and may null the whole distribution block).
        let json = r#"{"summary": {"productionDistribution": null}}"#;
        let r: DashboardEnergyResponse = serde_json::from_str(json).expect("no storage");
        assert_eq!(r.charged_watt_hours(), None);
        assert_eq!(r.discharged_watt_hours(), None);
    }
}
