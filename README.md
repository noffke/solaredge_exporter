# solaredge_exporter

Prometheus exporter for SolarEdge **per-optimizer** metrics (power, voltage,
current, lifetime energy). Complements a modbus-based exporter that handles
site/inverter/meter/battery level.

The public SolarEdge Monitoring API does not expose per-optimizer telemetry
and is rate-limited to 300 requests/day. This exporter instead uses the
undocumented portal endpoints at `monitoring.solaredge.com`, as pioneered by
the [ProudElm/solaredgeoptimizers][upstream-ha] Home Assistant integration.
The portal updates at ~15-min cadence and has no hard request budget.

[upstream-ha]: https://github.com/ProudElm/solaredgeoptimizers

## Configuration

### Environment variables

| Variable | Required | Purpose |
| --- | --- | --- |
| `SOLAREDGE_USERNAME` | yes | SolarEdge portal login |
| `SOLAREDGE_PASSWORD` | yes | SolarEdge portal password |
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

Operational metrics:

- `solaredge_portal_last_refresh_timestamp_seconds{kind="telemetry|energy"}`
- `solaredge_portal_refresh_duration_seconds{kind}`
- `solaredge_portal_refresh_errors_total{kind}`
- `solaredge_portal_login_total`

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
  -p 8888:8888 \
  solaredge_exporter
```

To change field mappings: edit `config.toml`, rebuild the image.

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
