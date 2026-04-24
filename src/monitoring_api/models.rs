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
    #[serde(default)]
    pub power: Option<f64>,
    #[serde(rename = "batteryState", default)]
    pub battery_state: Option<i64>,
    #[serde(rename = "lifeTimeEnergyCharged", default)]
    pub life_time_energy_charged: Option<f64>,
    #[serde(rename = "lifeTimeEnergyDischarged", default)]
    pub life_time_energy_discharged: Option<f64>,
    #[serde(rename = "fullPackEnergyAvailable", default)]
    pub full_pack_energy_available: Option<f64>,
    #[serde(rename = "internalTemp", default)]
    pub internal_temp: Option<f64>,
    #[serde(rename = "ACGridCharging", default)]
    pub ac_grid_charging: Option<f64>,
    #[serde(rename = "stateOfCharge", default)]
    pub state_of_charge: Option<f64>,
}

impl Battery {
    /// Returns the most recent telemetry entry (the API returns them in
    /// chronological order; the last one is current).
    pub fn latest_telemetry(&self) -> Option<&BatteryTelemetry> {
        self.telemetries.last()
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
                        {"timeStamp":"2026-04-23 09:00:00","power":0,"batteryState":3,"lifeTimeEnergyCharged":1000,"lifeTimeEnergyDischarged":800,"fullPackEnergyAvailable":8950,"internalTemp":25,"ACGridCharging":50,"stateOfCharge":42.5},
                        {"timeStamp":"2026-04-23 09:05:00","power":120,"batteryState":3,"lifeTimeEnergyCharged":1010,"lifeTimeEnergyDischarged":800,"fullPackEnergyAvailable":8950,"internalTemp":26,"ACGridCharging":60,"stateOfCharge":43.0}
                    ]
                }]
            }
        }"#;
        let r: StorageDataResponse = serde_json::from_str(json).expect("storage");
        let b = &r.storage_data.batteries[0];
        assert_eq!(b.serial_number, "BAT1");
        let t = b.latest_telemetry().expect("latest");
        assert_eq!(t.life_time_energy_charged, Some(1010.0));
        assert_eq!(t.state_of_charge, Some(43.0));
        assert_eq!(t.ac_grid_charging, Some(60.0));
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
