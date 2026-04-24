# solaredge_exporter

Prometheus exporter for SolarEdge **per-optimizer** metrics (power, voltage,
current, today's energy) **plus battery and site-level energy counters** that
neither modbus SunSpec nor the optimizer API exposes. Complements a modbus-based
exporter that handles site/inverter/meter-level power and SoC.

Two upstream sources are combined:

1. **Undocumented portal endpoints** at `monitoring.solaredge.com` (pioneered by
   [ProudElm/solaredgeoptimizers][upstream-ha]) — per-optimizer live telemetry.
   No hard request budget, refreshes every ~15 min.
2. **Public Monitoring API** at `monitoringapi.solaredge.com` — battery lifetime
   charged/discharged/grid-charging counters, site meter lifetime energy, and
   site PV lifetime energy. Rate-limited to **300 requests/day**; we poll
   three endpoints every 30 min by default (~144 calls/day).

[upstream-ha]: https://github.com/ProudElm/solaredgeoptimizers

## Configuration

### Environment variables

| Variable | Required | Purpose |
| --- | --- | --- |
| `SOLAREDGE_USERNAME` | yes | SolarEdge portal login (for the undocumented portal scrape) |
| `SOLAREDGE_PASSWORD` | yes | SolarEdge portal password |
| `SOLAREDGE_API_KEY`  | yes | Public Monitoring API key (Site Admin → Admin → Site Access → API Access in the portal) |
| `RUST_LOG`           | no (default `info`) | log level filter |

### CLI

```
solaredge_exporter --config <PATH>
```

### `config.toml`

See `config.toml.example`. `site_id` is the only required field; fields and
refresh interval have sensible defaults.

## Exposed metrics

Per-optimizer gauges (labels: `optimizer`, `display_name`, `inverter`, `field`):

- `solaredge_optimizer_power_watts`
- `solaredge_optimizer_module_voltage_volts` — voltage at the PV module terminals
- `solaredge_optimizer_dc_voltage_volts` — DC voltage at the optimizer output
- `solaredge_optimizer_current_amperes`
- `solaredge_optimizer_energy_today_watt_hours` — energy produced since the start of the current day. The portal's `/layout/energy?timeUnit=ALL` endpoint returns per-day values at the optimizer level even though the query parameter suggests otherwise; for true lifetime you can still read `solaredge_inverter_ac_energy_watt_hours` from the modbus exporter.
- `solaredge_optimizer_last_measurement_timestamp_seconds`

Battery gauges from the public Monitoring API (labels: `battery` = serial, `model`):

- `solaredge_battery_energy_charged_watt_hours` — lifetime energy charged into the battery
- `solaredge_battery_energy_discharged_watt_hours` — lifetime energy discharged from the battery
- `solaredge_battery_ac_grid_charging_watt_hours_total` — **counter** of AC energy used to charge the battery from the grid. The API returns this as a windowed sum; the exporter tracks the last successful query timestamp and queries the exact interval since, so successive responses contribute non-overlapping deltas. **Persisted across restarts** when `monitoring_api.state_file` is set (see "Persistent state" below). Counter is seeded on first run with the last 24 h and then accumulates.
- `solaredge_battery_full_pack_energy_watt_hours` — current maximum storable energy; divide by the nameplate value for State-of-Health
- `solaredge_battery_state_of_charge_percent`
- `solaredge_battery_power_watts` — positive = charging, negative = discharging
- `solaredge_battery_internal_temperature_celsius`
- `solaredge_battery_state` — enum: 0 Invalid, 1 Standby, 2 Thermal Mgmt, 3 Enabled, 4 Fault

Site meter lifetime counters (labels: `meter`, `inverter`, `type`):

- `solaredge_monitoring_meter_lifetime_energy_watt_hours{type="Production|Consumption|FeedIn|Purchased"}`

The `monitoring_` infix distinguishes these from the modbus-sourced `solaredge_meter_*` series, so dashboards can pick whichever side is authoritative.

Site PV lifetime (no labels):

- `solaredge_site_pv_lifetime_energy_watt_hours` — total PV production since site commissioning. Day/month/year totals are derivable as `increase(solaredge_site_pv_lifetime_energy_watt_hours[1d|30d|365d])` in PromQL.

### Derived Production / Consumption / SelfConsumption

When the site has only a grid meter (the common case), the public API returns
`Purchased` and `FeedIn` meters but not `Production` or `Consumption`. Derive
them in Prometheus — see `recording-rules.example.yml` for a drop-in rules
file. The identities are:

```
Production      = solaredge_site_pv_lifetime_energy_watt_hours
SelfConsumption = Production − FeedIn
Consumption     = Production − FeedIn + Purchased
```

#### Installing the rules

**1. Copy the file to a directory Prometheus can read.**

Typical layouts:

| Setup | Path |
| --- | --- |
| bare-metal | `/etc/prometheus/rules/solaredge.rules.yml` |
| Docker bind-mount | `./prometheus/rules/solaredge.rules.yml` next to your `prometheus.yml`, mounted into the container |

**2. Reference the rules directory from `prometheus.yml`:**

```yaml
rule_files:
  - /etc/prometheus/rules/*.yml
```

**3. Validate before reloading** (catches YAML typos and query errors):

```sh
promtool check rules /etc/prometheus/rules/solaredge.rules.yml
# or, from inside a docker container:
docker exec prometheus promtool check rules /etc/prometheus/rules/solaredge.rules.yml
```

**4. Reload Prometheus** so it picks the new rules up without a full restart:

```sh
# If Prometheus was started with --web.enable-lifecycle:
curl -X POST http://localhost:9090/-/reload

# Otherwise, send SIGHUP:
kill -HUP $(pidof prometheus)
# Docker equivalent (Prometheus usually runs as PID 1 in the container):
docker exec prometheus kill -HUP 1
# or with Compose:
docker compose kill -s HUP prometheus
```

**5. Verify** at <http://localhost:9090/rules> — the `solaredge_derived` group
should be listed with state `ok` and a `Last Evaluation` timestamp. Then
query `solaredge_derived_lifetime_energy_watt_hours` in the UI's graph tab;
you should see three series, one for each `type`.

#### Minimal Docker Compose snippet

If you're starting from scratch:

```yaml
services:
  prometheus:
    image: prom/prometheus:latest
    command:
      - '--config.file=/etc/prometheus/prometheus.yml'
      - '--web.enable-lifecycle'
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - ./rules:/etc/prometheus/rules:ro
      - prometheus_data:/prometheus
    ports:
      - "9090:9090"

volumes:
  prometheus_data:
```

Drop `recording-rules.example.yml` into `./rules/solaredge.rules.yml` and
point `prometheus.yml` at `/etc/prometheus/rules/*.yml`.

Operational metrics:

- `solaredge_portal_last_refresh_timestamp_seconds{kind="telemetry|energy"}`
- `solaredge_portal_refresh_duration_seconds{kind}`
- `solaredge_portal_refresh_errors_total{kind}`
- `solaredge_portal_login_total`
- `solaredge_monitoring_api_last_refresh_timestamp_seconds{endpoint="overview|meters|storage"}`
- `solaredge_monitoring_api_refresh_duration_seconds{endpoint}`
- `solaredge_monitoring_api_refresh_errors_total{endpoint}`
- `solaredge_monitoring_api_requests_total{endpoint}` — watch `increase(...[24h])` against the 300/day cap

## Bootstrapping field mappings

On startup the exporter fetches the site layout once and logs every discovered
optimizer (inverter serial, optimizer serial, display name, reporter ID) at
`INFO`. Run it once with an empty `[[fields]]` list, grep the log for
`"optimizer"` entries, copy serials into `config.toml`, and restart.

Optimizers not listed in any field are still exported with label
`field="unassigned"`, so nothing is silently dropped.

## Run

### Local

```sh
SOLAREDGE_USERNAME=you@example.com \
SOLAREDGE_PASSWORD=hunter2 \
SOLAREDGE_API_KEY=L4QLVQ1L… \
cargo run -- --config config.toml
curl -s localhost:8888/metrics
```

### Docker

`config.toml` is copied into the image at build time, so you must have it
present locally before building (the repo ships with `config.toml.example`
as a template).

```sh
docker build -t solaredge_exporter .
docker run --rm \
  -e SOLAREDGE_USERNAME=you@example.com \
  -e SOLAREDGE_PASSWORD=hunter2 \
  -e SOLAREDGE_API_KEY=L4QLVQ1L… \
  -p 8888:8888 \
  solaredge_exporter
```

To change field mappings: edit `config.toml`, rebuild the image.

## Persistent state

The `solaredge_battery_ac_grid_charging_watt_hours_total` counter is the only
value that has to survive process restarts — every other metric is either
re-derived from a fresh API call each cycle, or comes from a true lifetime
counter inside the battery itself. To avoid losing this counter on container
restarts (reboot, image update, OOM), point `monitoring_api.state_file` at a
JSON file inside a mounted volume:

```toml
[monitoring_api]
state_file = "/state/state.json"
```

```sh
docker run --rm \
  -v solaredge_state:/state \
  -e SOLAREDGE_USERNAME=… -e SOLAREDGE_PASSWORD=… -e SOLAREDGE_API_KEY=… \
  -p 8888:8888 \
  solaredge_exporter
```

The file is written atomically (`write` + `rename`) after every successful
`storageData` fetch, so a crash mid-write can only lose one refresh cycle's
delta. At startup the counter is seeded to the persisted value before the
HTTP server accepts any scrape — Prometheus sees a clean reset with no
spurious `increase()` spike. If the state file is corrupt or unreadable,
the exporter logs a WARN and falls back to a runtime-only counter for
that session.

Leave `state_file` unset for a stateless smoke-test run; a WARN at startup
flags that the counter will reset on exit.

## Debugging portal responses

Every successful portal response is logged at `DEBUG` with the full body.
Transport-layer debug logs from `hyper`/`h2`/`reqwest` are very noisy, so
target just this crate when investigating API drift:

```sh
RUST_LOG=info,solaredge_exporter=debug cargo run -- --config config.toml
```

`login`, `layout/logical`, `systemData` (one per optimizer), and `layout/energy`
bodies will appear verbatim — exactly what to diff against when an endpoint
changes shape.

## Upstream references

**Update here first when the portal API changes.** The HTTP logic is a Rust
port of a Python library; if SolarEdge changes an endpoint, diff against the
upstream Python file to see what moved.

- HA integration (entry point): <https://github.com/ProudElm/solaredgeoptimizers>
- PyPI library with the actual HTTP logic:
  <https://github.com/ProudElm/packaging_solaredgeoptimizers/blob/main/src/solaredgeoptimizers/solaredgeoptimizers.py>

### Ported from upstream commit

`0278ba2fd19feff62994660e68387c07c3494235` (dated 2023-04-21).

To see what upstream has changed since the port:

```sh
git -C /tmp clone https://github.com/ProudElm/packaging_solaredgeoptimizers
git -C /tmp/packaging_solaredgeoptimizers \
    diff 0278ba2fd19feff62994660e68387c07c3494235 HEAD \
    -- src/solaredgeoptimizers/solaredgeoptimizers.py
```

### Endpoints (as currently used)

All on `monitoring.solaredge.com` (the old `monitoringpublic` host returns 403
for `publicSystemData` as of 2026-04; the migration is tracked in upstream
[PR #13]).

| Endpoint | Auth | Purpose |
| --- | --- | --- |
| `GET /solaredge-web/p/login` | Basic | warm session cookies (JSESSIONID, CSRF-TOKEN) |
| `GET /solaredge-apigw/api/sites/{siteId}/layout/logical` | Basic | inverter → string → optimizer tree |
| `GET /solaredge-web/p/systemData?reporterId=…&type=panel&fieldId={siteId}&isPublic=false&v={ms}` | Basic | per-optimizer live measurements |
| `POST /solaredge-apigw/api/sites/{siteId}/layout/energy?timeUnit=ALL` | Basic + CSRF cookie + `Content-Type: application/json` | per-optimizer lifetime energy |

[PR #13]: https://github.com/ProudElm/packaging_solaredgeoptimizers/pull/13
