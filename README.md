# solaredge_exporter

Prometheus exporter for SolarEdge **per-optimizer** metrics (power, voltage,
current, today's energy) **plus battery and site-level energy counters** that
neither modbus SunSpec nor the optimizer API exposes. Complements a modbus-based
exporter that handles site/inverter/meter-level power and SoC.

Two upstream sources are combined:

1. **Undocumented portal endpoints** at `monitoring.solaredge.com` — the
   SolarEdge ONE `/services/` API (pioneered by
   [AndrewTapp/solaredgeoptimizers][upstream-ha]) — per-optimizer live telemetry,
   plus site-level battery charge/discharge energy from the dashboard energy
   service. No hard request budget, refreshes every ~15 min. The older
   `/solaredge-apigw/` + `/solaredge-web/` endpoints this project originally used
   were retired by SolarEdge in July 2026; see
   [the shutdown notes](#the-july-2026-legacy-api-shutdown).
2. **Public Monitoring API** at `monitoringapi.solaredge.com` — battery
   grid-charging counter, current full-pack energy / power / temp / state,
   site meter lifetime energy, and site PV lifetime energy. Rate-limited to
   **300 requests/day**; we poll three endpoints every 30 min by default
   (~144 calls/day).

[upstream-ha]: https://github.com/AndrewTapp/solaredgeoptimizers

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

Site-level battery charge/discharge energy (no labels), sourced from the portal
dashboard energy endpoint — the public API reports these as 0 for the SolarEdge
Home Battery 48V:

- `solaredge_battery_energy_charged_watt_hours` — cumulative energy charged into the battery from PV (excludes grid charging, tracked separately below)
- `solaredge_battery_energy_discharged_watt_hours` — cumulative energy discharged from the battery to the home

Battery gauges from the public Monitoring API (labels: `battery` = serial, `model`):

- `solaredge_battery_ac_grid_charging_watt_hours_total` — **counter** of AC energy used to charge the battery from the grid. The API returns this as a windowed sum; the exporter tracks the last successful query timestamp and queries the exact interval since, so successive responses contribute non-overlapping deltas. **Persisted across restarts** when `monitoring_api.state_file` is set (see "Persistent state" below). Counter is seeded on first run with the last 24 h and then accumulates.
- `solaredge_battery_full_pack_energy_watt_hours` — current maximum storable energy; divide by the nameplate value for State-of-Health
- `solaredge_battery_state` — enum: 0 Invalid, 1 Standby, 2 Thermal Mgmt, 3 Enabled, 4 Fault

State of charge is **not** emitted here — the companion modbus exporter serves
`solaredge_battery_state_of_charge_percent` from modbus at higher resolution.
Instantaneous battery **power** and **temperature** are likewise not emitted
here: the modbus/sunspec exporter publishes `solaredge_battery_power_watts` and
`solaredge_battery_temperature_celsius` in real time (1 min) from the proprietary
battery register block, so the portal's hourly equivalents were dropped.

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
you should see three series, one for each `type`. Note that `health: ok`
on the rules page only means the query parsed and ran — it does *not*
guarantee samples were emitted. Always confirm by querying the metric.

### Cumulative euro-savings counter

The same `recording-rules.example.yml` file ships a second group,
`solaredge_savings`, that turns `SelfConsumption` into a running eurocent
counter by multiplying each per-tick energy delta by a grid-price metric
exposed by another exporter (assumed name:
`energy_price_exporter_price_per_kwh_eurocent`). The integration is
price-aware, so a future time-of-use tariff is handled correctly without
changing the rule.

Recorded series:

| Metric | Unit | Notes |
| --- | --- | --- |
| `pv:self_consumption_lifetime_kwh` | kWh | Inverter AC energy − grid export, modbus-sourced (5-min) |
| `pv:battery_self_consumption_lifetime_kwh` | kWh | `solaredge_battery_energy_discharged_watt_hours / 1000` |
| `pv:direct_self_consumption_lifetime_kwh` | kWh | total − battery (battery losses absorbed here) |
| `pv:savings_total_eurocent` | eurocent | Cumulative — survives Prometheus restarts up to 14 days (see below) |
| `pv:savings_battery_total_eurocent` | eurocent | Battery's contribution to savings |
| `pv:savings_direct_total_eurocent` | eurocent | Direct PV→load contribution |
| `pv:savings_rate_eurocent_per_hour` | eurocent/h | Instantaneous; emits no sample across a >5 min Prometheus gap rather than a spurious spike |

The previous-value lookup uses `max_over_time(metric[14d] offset 30s)`
rather than `metric offset 30s`. Because both the input lifetime kWh and
the savings counter are monotonic, this returns the prior sample in
normal operation and the pre-gap peak after a Prometheus restart of up
to 14 days. The gap's energy delta is priced at the post-restart price,
which loses some accuracy on a TOU tariff but never zeros the counter.
The 14 d range bounds per-eval scan cost so it stays flat as your TSDB
retention grows; raise it if you expect longer outages.

The one trade-off: if the underlying lifetime kWh counter ever resets to
a lower value (inverter replacement, modbus state corruption), the rule
will freeze its `_total` outputs until the new counter climbs back past
the old peak. To force a clean restart, delete the affected series via
the admin API:

```sh
curl -X POST -g 'http://localhost:9090/api/v1/admin/tsdb/delete_series?match[]=pv:savings_total_eurocent'
curl -X POST 'http://localhost:9090/api/v1/admin/tsdb/clean_tombstones'
```

(Requires `--web.enable-admin-api` on the Prometheus binary.)

Cross-check while the price is flat:

```promql
pv:savings_total_eurocent
≈
pv:self_consumption_lifetime_kwh * scalar(energy_price_exporter_price_per_kwh_eurocent)
```

Adjust the `{job="..."}` selectors at the top of the rules file if your
`prometheus.yml` uses different scrape job names — `solaredge_exporter` for
this exporter and `solaredge_sunspec_exporter` for the modbus side. Without
a matching `job` pin the rule will silently emit no samples (because the
selector matches nothing) or double-count (if multiple jobs scrape the same
exporter).

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
optimizer (inverter serial, optimizer serial, display name, layout status) at
`INFO`. Run it once with an empty `[[fields]]` list, grep the log for
`"optimizer"` entries, copy serials into `config.toml`, and restart.

Optimizers not listed in any field are still exported with label
`field="unassigned"`, so nothing is silently dropped.

If the layout fetch fails, the exporter logs the error, increments
`solaredge_refresh_errors_total{kind="layout"}` and **starts anyway** with no
optimizer metrics — the battery, meter and site-PV metrics come from the separate
public API and are unaffected.

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

## Monitoring & alerting

Because this exporter scrapes *unofficial* SolarEdge APIs, they can break
silently (HTTP 200 while the data quietly stops). Ready-to-use Prometheus alert
rules live in [`monitoring/solaredge_monitoring.rules.yml`](monitoring/solaredge_monitoring.rules.yml):
exporter-down, per-source data staleness (the main "an API changed" signal),
refresh-error counters, a battery-charge/discharge-unavailable alert, and a
guard on the public API's 300/day budget. They carry `severity: warning|critical`
labels for Alertmanager routing.

Deploy:

```yaml
# prometheus.yml
rule_files:
  - /etc/prometheus/rules/solaredge_monitoring.rules.yml   # copy the file here
```

Then reload Prometheus (see the SIGHUP/lifecycle note above). Routing to a
receiver is handled by your existing Alertmanager config.

> **Important — scrape interval.** Set this job's `scrape_interval` to **≤ 2m**
> (60s recommended), *not* 15m. The exporter refreshes upstream in the
> background; `/metrics` just serves the cached values, so frequent scrapes cost
> nothing (no upstream calls, no API budget). A 15m `scrape_interval` exceeds
> Prometheus's 5m instant-query lookback, which makes every series "stale"
> between scrapes — invisible to instant queries, dashboards, and most alert
> evaluations. The shipped rules use `*_over_time([20m])` windows so they still
> work at 15m, but a normal interval is strongly preferred and lets you tighten
> the staleness thresholds.

### Reacting to an alert — log mapping

Every alert that can fire from a runtime condition has a matching `WARN` log
line (default `RUST_LOG=info` shows them), so when one fires you can grep the
exporter logs and see *why* immediately. The trip conditions and their logs:

| Alert | Log line (level `WARN`) |
| --- | --- |
| `SolarEdgeExporterDown` / `…Absent` | *(none — the process is dead/unreachable; absence of logs is the signal)* |
| `SolarEdgePortalDataStale{kind="telemetry"}` | `no optimizer telemetry committed this cycle…` (and/or `fetch_optimizers_live failed…` on HTTP errors) |
| `SolarEdgePortalDataStale{kind="energy"}` | `fetch_optimizer_energy failed; leaving its energy gauge unchanged` |
| `SolarEdgePortalDataStale{kind="battery_energy"}` / `SolarEdgeBatteryEnergyMissing` | `fetch_battery_energy failed or returned no usable data…` (HTTP/Cognito error, **or** a parsed-but-empty 200 — the error then carries the response body) |
| `SolarEdgeOptimizerMetricsMissing` / `SolarEdgePortalRefreshErrors{kind="layout"}` | `fetch_site_structure failed; continuing WITHOUT optimizer metrics…` — the startup layout fetch failed, so there are no optimizer series at all (by design: the other metrics stay up). The `{kind="layout"}` counter only moves once at startup, so the `…MetricsMissing` rule is what keeps alerting. |
| `SolarEdgePortalRefreshErrors{kind}` | the per-source lines above (layout / optimizer / energy / battery_energy) |
| `SolarEdgeMonitoringApiRefreshErrors{endpoint}` / `…DataStale` | `monitoring_api fetch failed or returned no usable data…` (carries the `endpoint` field; **EmptyResponse** errors include the body) |
| `SolarEdgeApiBudgetHigh` | startup `monitoring_api.refresh_seconds is low…` |

The silent-drift cases (HTTP 200 but the data stopped) are the important ones.
The exporter detects them by validating each response and, when a 200 parses but
lacks the expected fields, raising an **`EmptyResponse` error that carries the
(truncated) response body** — so the `WARN` already contains the body you need to
diff, no reconfiguration required. It also only advances each source's
`…_last_refresh_timestamp_seconds` on a real value, so the staleness alert fires.
For the *full* body, raise this crate's log level — see below.

## Debugging portal responses

Every response (portal **and** public Monitoring API) is logged at `DEBUG` with
the full body. Transport-layer debug logs from `hyper`/`h2`/`reqwest` are very
noisy, so target just this crate when investigating API drift:

```sh
RUST_LOG=info,solaredge_exporter=debug cargo run -- --config config.toml
# Docker: add  -e RUST_LOG=info,solaredge_exporter=debug
```

`layout/v2`, `layout/optimizers`, `layout/energy-graph` (one per optimizer),
`dashboard/energy`, and the public API's `overview`/`meters`/`storage` bodies
appear verbatim — exactly what to diff against when an endpoint changes shape.
Those are the `endpoint=` field values on each logged response, and the same
labels appear in `PortalError`, so a WARN names the endpoint that broke.

**Capturing a body after an alert fires.** You don't need DEBUG enabled *before*
a break: an API that changed stays changed, so just raise the level and wait one
refresh cycle (≤15 min portal, ≤30 min public API) to capture the still-broken
response. For the parsed-but-empty (`EmptyResponse`) cases, the truncated body is
already in the default-level `WARN`, so often you don't even need this step.

## Upstream references

**Update here first when the portal API changes.** The HTTP logic is a Rust
port of a Python library; if SolarEdge changes an endpoint, diff against the
upstream Python file to see what moved.

- Active fork, implements the same `/services/` API we target:
  <https://github.com/AndrewTapp/solaredgeoptimizers>
- The HTTP logic lives in
  <https://github.com/AndrewTapp/solaredgeoptimizers/blob/main/custom_components/solaredgeoptimizers/solaredge_one_api.py>
- Original project the first version was ported from, **dormant since 2023 and
  never fixed for the July 2026 shutdown** — kept for history only:
  <https://github.com/ProudElm/solaredgeoptimizers>

### Ported from upstream commit

`9f1376bd2553a8b60ad762e2606441079030e0ba` (dated 2026-07-03).

To see what upstream has changed since the port:

```sh
git -C /tmp clone https://github.com/AndrewTapp/solaredgeoptimizers
git -C /tmp/solaredgeoptimizers \
    diff 9f1376bd2553a8b60ad762e2606441079030e0ba HEAD \
    -- custom_components/solaredgeoptimizers/solaredge_one_api.py
```

### The July 2026 legacy-API shutdown

On ~2026-07-21 SolarEdge retired the old portal API without notice.
`GET /solaredge-apigw/api/sites/{siteId}/layout/logical` returns **HTTP 410 Gone**
with a body containing only
`https://marketing.solaredge.com/legacy-api-sign-up-for-interest`, and
`/solaredge-web/p/login` now 301s to the new `/one` SPA. The 410 is served at the
CDN edge *before* authentication, so it is not a credentials problem.

This exporter has moved to the **SolarEdge ONE `/services/` API**, which is gated
by the same AWS Cognito app client we already authenticated against for the
battery-energy endpoint — so the migration needed no new credentials or setup.
Per-optimizer data is now keyed by **serial number**; the old numeric
`reporterId` no longer exists anywhere in the API or this codebase.

Separately, the **public** Monitoring API (`monitoringapi.solaredge.com`) is
announced for deprecation on **2026-11-01** per
<https://api-docs.solaredge.com/>, to be replaced by an OAuth-based V2 API on the
ONE platform. No V2 spec is published yet. The battery, meter and site-PV metrics
here depend on V1.

### Endpoints (as currently used)

All on `monitoring.solaredge.com`, all authenticated with the Cognito access
token (sent both as an `Authorization: Bearer` header and as the
`se_monitoring_auth` cookie). HTTP Basic auth is no longer used anywhere.

| Endpoint | Purpose |
| --- | --- |
| `GET /services/layout/logical/generic/v2/site/{siteId}?include-optimizers=true` | inverter → string → optimizer tree |
| `POST /services/layout/information/optimizers` (body: `["<serial>", …]`) | live measurements for **every** optimizer in one call |
| `GET /services/layout/energy-graph/site/{siteId}/optimizers?…&optimizer-serials=…` | per-optimizer lifetime energy (one call per optimizer — `totalEnergy` is a single scalar for whatever serials it is given) |
| `GET /services/dashboard/energy/sites/{siteId}?…` | site battery charge/discharge energy |
