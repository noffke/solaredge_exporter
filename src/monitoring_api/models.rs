use serde::Deserialize;

// ===== /site/{id}/overview =====

#[derive(Debug, Deserialize)]
pub struct OverviewResponse {
    #[serde(default)]
    pub overview: Overview,
}

#[derive(Debug, Default, Deserialize)]
pub struct Overview {
    #[serde(rename = "lifeTimeData", default)]
    pub life_time_data: EnergyValue,
}

#[derive(Debug, Default, Deserialize)]
pub struct EnergyValue {
    #[serde(default)]
    pub energy: Option<f64>,
}

// ===== /site/{id}/meters =====

#[derive(Debug, Deserialize)]
pub struct MetersResponse {
    #[serde(rename = "meterEnergyDetails", default)]
    pub meter_energy_details: MeterEnergyDetails,
}

#[derive(Debug, Default, Deserialize)]
pub struct MeterEnergyDetails {
    #[serde(default)]
    pub meters: Vec<Meter>,
}

#[derive(Debug, Deserialize)]
pub struct Meter {
    #[serde(rename = "meterSerialNumber", default)]
    pub meter_serial_number: String,
    #[serde(rename = "connectedSolaredgeDeviceSN", default)]
    pub connected_solaredge_device_sn: String,
    #[serde(rename = "meterType", default)]
    pub meter_type: String,
    #[serde(default)]
    pub values: Vec<MeterValue>,
}

#[derive(Debug, Deserialize)]
pub struct MeterValue {
    #[serde(default)]
    pub value: Option<f64>,
}

impl Meter {
    /// Returns the most recent non-null lifetime energy reading. The API
    /// occasionally returns `{"date": "..."}` entries without a `value` field
    /// (and sometimes with an explicit `null`); skip those and walk backwards
    /// until a real number appears.
    pub fn latest_value(&self) -> Option<f64> {
        self.values.iter().rev().find_map(|v| v.value)
    }
}

// ===== /site/{id}/storageData =====

#[derive(Debug, Deserialize)]
pub struct StorageDataResponse {
    #[serde(rename = "storageData", default)]
    pub storage_data: StorageData,
}

#[derive(Debug, Default, Deserialize)]
pub struct StorageData {
    #[serde(default)]
    pub batteries: Vec<Battery>,
}

#[derive(Debug, Deserialize)]
pub struct Battery {
    #[serde(rename = "serialNumber", default)]
    pub serial_number: String,
    #[serde(rename = "modelNumber", default)]
    pub model_number: String,
    #[serde(default)]
    pub telemetries: Vec<BatteryTelemetry>,
}

#[derive(Debug, Deserialize)]
pub struct BatteryTelemetry {
    #[serde(rename = "batteryState", default)]
    pub battery_state: Option<i64>,
    // lifeTimeEnergyCharged / lifeTimeEnergyDischarged are intentionally not
    // modelled: the public storageData API reports them as 0 for the SolarEdge
    // Home Battery 48V. Charge/discharge energy comes from the portal dashboard
    // endpoint instead (see src/portal/, src/scrape.rs).
    #[serde(rename = "fullPackEnergyAvailable", default)]
    pub full_pack_energy_available: Option<f64>,
    #[serde(rename = "ACGridCharging", default)]
    pub ac_grid_charging: Option<f64>,
}

impl Battery {
    /// Most recent non-null value of `field` across all telemetries.
    /// Mirrors [`Meter::latest_value`] — the storage endpoint can return
    /// trailing entries where a given field is null while earlier
    /// telemetries in the same response carry a real number, so we walk
    /// back to the freshest populated sample per field.
    pub fn latest<T: Copy>(&self, field: impl Fn(&BatteryTelemetry) -> Option<T>) -> Option<T> {
        self.telemetries.iter().rev().find_map(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_overview_response() {
        let json = r#"{
            "overview": {
                "lastUpdateTime": "2026-04-23 10:00:00",
                "lifeTimeData": {"energy": 74353.0, "revenue": 0.0},
                "lastYearData": {"energy": 0.0},
                "lastMonthData": {"energy": 0.0},
                "lastDayData": {"energy": 0.0},
                "currentPower": {"power": 123.4}
            }
        }"#;
        let r: OverviewResponse = serde_json::from_str(json).expect("overview");
        assert_eq!(r.overview.life_time_data.energy, Some(74353.0));
    }

    #[test]
    fn parses_meters_response_and_picks_latest() {
        let json = r#"{
            "meterEnergyDetails": {
                "timeUnit": "DAY",
                "unit": "Wh",
                "meters": [
                    {
                        "meterSerialNumber": "S1",
                        "connectedSolaredgeDeviceSN": "INV1",
                        "model": "X",
                        "meterType": "Production",
                        "values": [
                            {"date": "2026-04-22 00:00:00", "value": 100.0},
                            {"date": "2026-04-23 00:00:00", "value": 200.0},
                            {"date": "2026-04-24 00:00:00"}
                        ]
                    }
                ]
            }
        }"#;
        let r: MetersResponse = serde_json::from_str(json).expect("meters");
        let m = &r.meter_energy_details.meters[0];
        assert_eq!(m.meter_type, "Production");
        assert_eq!(m.latest_value(), Some(200.0));
    }

    #[test]
    fn parses_storage_response() {
        let json = r#"{
            "storageData": {
                "batteryCount": 1,
                "batteries": [{
                    "serialNumber": "BAT1",
                    "modelNumber": "LGXXXXX",
                    "telemetryCount": 2,
                    "telemetries": [
                        {"timeStamp":"2026-04-23 09:00:00","power":0,"batteryState":3,"fullPackEnergyAvailable":8950,"internalTemp":25,"ACGridCharging":50},
                        {"timeStamp":"2026-04-23 09:05:00","power":120,"batteryState":3,"fullPackEnergyAvailable":8950,"internalTemp":26,"ACGridCharging":60}
                    ]
                }]
            }
        }"#;
        let r: StorageDataResponse = serde_json::from_str(json).expect("storage");
        let b = &r.storage_data.batteries[0];
        assert_eq!(b.serial_number, "BAT1");
        assert_eq!(b.latest(|t| t.full_pack_energy_available), Some(8950.0));
        assert_eq!(b.latest(|t| t.ac_grid_charging), Some(60.0));
    }

    #[test]
    fn battery_walks_back_to_latest_non_null_per_field() {
        // Latest telemetry omits ACGridCharging (the storage endpoint can
        // return trailing entries where a field is null while earlier ones
        // carry a real number); earlier telemetry has it. `latest()` must pick
        // the earlier value up while still preferring the freshest sample for
        // fields that *are* present in the latest entry.
        let json = r#"{
            "storageData": {
                "batteries": [{
                    "serialNumber": "BAT1",
                    "modelNumber": "SolarEdge Home Battery 48V (W) ",
                    "telemetries": [
                        {"timeStamp":"2026-04-23 09:00:00","power":0,"batteryState":3,
                         "ACGridCharging":120,
                         "fullPackEnergyAvailable":8950,
                         "internalTemp":25},
                        {"timeStamp":"2026-04-23 09:05:00","power":120,"batteryState":3,
                         "internalTemp":26,"fullPackEnergyAvailable":8950}
                    ]
                }]
            }
        }"#;
        let r: StorageDataResponse = serde_json::from_str(json).expect("storage");
        let b = &r.storage_data.batteries[0];
        assert_eq!(b.latest(|t| t.ac_grid_charging), Some(120.0));
        assert_eq!(b.latest(|t| t.battery_state), Some(3));
        assert_eq!(b.latest(|t| t.full_pack_energy_available), Some(8950.0));
    }

    #[test]
    fn tolerates_missing_fields() {
        let r: OverviewResponse = serde_json::from_str(r#"{"overview":{}}"#).expect("empty");
        assert_eq!(r.overview.life_time_data.energy, None);
        let r: StorageDataResponse =
            serde_json::from_str(r#"{"storageData":{"batteries":[]}}"#).expect("no batts");
        assert!(r.storage_data.batteries.is_empty());
    }
}
