# CLAUDE.md

Project-specific guidance for Claude Code when working in this repository.

## What this is

Rust Prometheus exporter combining two SolarEdge data sources:

1. **Undocumented portal scrape** (`src/portal/`) — per-optimizer live
   telemetry, plus **site-level battery charge/discharge energy** from the
   dashboard energy service (see below). Targets the **SolarEdge ONE
   `/services/` API** on `monitoring.solaredge.com`; the original
   `ProudElm/packaging_solaredgeoptimizers` endpoints were retired in July 2026
   (see "Portal endpoints"). No request budget; refreshes every 15 min.
2. **Public Monitoring API** (`src/monitoring_api/`) — battery
   `ACGridCharging`, `fullPackEnergyAvailable`, power/temp/state,
   site meter lifetime energy, and site PV lifetime energy. Hand-rolled
   against `monitoringapi.solaredge.com` (we don't use the `solaredge`
   crate — its `http-adapter` transitively pins `reqwest 0.12` and we're
   on 0.13). **Hard-capped at 300 req/day**; refreshes every 30 min with
   three calls per cycle (~144 calls/day).

   ⚠️ **Announced for deprecation on 2026-11-01.** `https://api-docs.solaredge.com/`
   (content dated 2026-07-23) states the V1 Monitoring API is "planned for
   deprecation on November 1, 2026 as the V2 API on the SolarEdge ONE platform
   takes over". V2 is OAuth-based with tiered pricing; **no public spec has been
   published yet**, so there is nothing to port to today. Battery, meter and
   site-PV metrics all depend on V1, so this needs revisiting before November.

Complements a separate modbus-based exporter (inverter/meter/DER live power,
battery SoC). Don't duplicate those metrics here — battery SoC
(`solaredge_battery_state_of_charge_percent`) is intentionally **not** emitted
by this exporter; the modbus exporter serves it from modbus at higher
resolution. Add new public-API metrics only when they plug a genuine gap.

**Battery charge/discharge energy is sourced from the portal, not the public
API.** The public `storageData` endpoint reports `lifeTimeEnergyCharged` /
`lifeTimeEnergyDischarged` as `0` for the SolarEdge Home Battery 48V (LG-cell
batteries aggregate these from deltas; SolarEdge's own docs note the result is
"incomplete"). So `solaredge_battery_energy_charged_watt_hours` /
`_discharged_watt_hours` are **label-less site-level gauges** populated from the
dashboard energy endpoint's `productionToBattery` / `consumptionFromBattery`.
Caveat: `productionToBattery` is PV→battery only and excludes grid charging
(tracked separately by `battery_ac_grid_charging_watt_hours`).

**These lifetime gauges are modeled as label-less `Family`s (key `NoLabels`),
not plain `Gauge`s, so they are _absent until the first successful fetch_ rather
than defaulting to `0`.** A plain `Gauge` is registered at startup and scraped at
`0` from process start until the first portal fetch lands; on a **lifetime**
total that `0` poisons `increase()`/`rate()`, because PromQL's counter-reset
compensation reads the restart transition `350k → 0 → 351k` as a reset and adds
the full pre-reset ~350k to the window — fabricating a day of impossible
charge/discharge (`increase([24h])` ≈ the whole lifetime). Absence has no
downward step, so the math stays honest. **Do not "fix" this by renaming to a
`_total` counter — the same compensation applies regardless of metric type or
name.** `site_pv_lifetime_energy` (and the already-`Family` meter lifetime) use
the same pattern; the `lifetime_gauges_absent_until_set` test in `metrics.rs`
locks it in. (Historical `0`-dips already in the TSDB are not rewritten; the fix
is forward-only.)

Absent-until-set only guards the **restart-to-0** dip. A **mid-run** downward
step — SolarEdge's upstream lifetime aggregate recomputing backward for a few
hours before recovering — would still emit a phantom reset. So every lifetime
gauge is written through `metrics::set_lifetime_monotonic`, which clamps the
value to the gauge's own running max (never storing a value below the highest
seen) and logs a WARN when it suppresses a downward step. This covers
`site_pv_lifetime_energy`, `monitoring_meter_lifetime_energy`, the portal
`battery_energy_charged` / `_discharged`, and the per-optimizer
`optimizer_energy_today`; the `lifetime_clamp_holds_on_downward_step` and
`lifetime_clamp_is_per_label_set` tests lock it in. No new state is needed (the
gauge is the max store), so the clamp is in-memory only — after a restart the
running max resets, but absent-until-set handles the clean restart and a restart
landing inside a rare dip window self-heals on recovery.

**`solaredge_optimizer_energy_today_watt_hours` is misnamed — it is a *lifetime*
total, not a daily one.** The retired endpoint was queried with `timeUnit=ALL`
and the replacement returns `totalEnergy`; both are cumulative since install.
That is why it goes through the clamp. It is deliberately **not** renamed: the
name is load-bearing for existing dashboards and recording rules, and a rename
buys nothing functionally. Note the clamp's running max is per label set, so one
optimizer's total never floors another's.

## Repo conventions (from `context.md`)

- **Never `git commit`.** The user creates all commits. You may and should
  `git add` new files as you create them.
- **Never `unwrap()`** on fallible values. Use `?` with typed errors (`thiserror`
  in libraries, `anyhow` in `main`). `.expect("msg")` is allowed only for
  invariants that cannot fail (e.g. deserialising a `const` fixture in a test).
- **Logs use local time and human-readable timestamps.** The `LocalTime`
  formatter in `src/main.rs` renders `%Y-%m-%d %H:%M:%S %:z` via `jiff`. Match
  this format if you add another logger.
- **Never log the password.** `portal::Secret` redacts in `Debug`. Don't print
  credentials elsewhere.

## Tech stack (pinned full versions in `Cargo.toml`)

- async runtime: `tokio`
- HTTP client: `reqwest` (rustls, no default TLS), with `Jar` cookie store
- HTTP server: `axum`
- metrics: `prometheus-client` (OpenMetrics)
- config: `serde` + `toml`
- CLI: `clap` derive, single `--config <PATH>` flag
- logging: `tracing` + `tracing-subscriber`
- time: `jiff` (not `chrono`, not `time`)
- errors: `thiserror` in modules, `anyhow` in `main`
- Cognito SRP login: `aws-cognito-srp` (computes the SRP handshake for the
  `/services/` dashboard auth; we do the HTTP ourselves — see `portal/cognito.rs`)

Prefer these when extending the code. Don't use `chrono`, `log`, `hyper`, or
`native-tls` **directly** in our code (use `jiff`/`tracing`/`reqwest`/rustls).
Note: `chrono`, `log`, and `regex` now arrive *transitively* via
`aws-cognito-srp` — that's accepted (the "no chrono" rule was a
direct-usage/consistency preference, not a hard dependency ban), but keep using
`jiff` for our own time handling.

## Runtime

- `SOLAREDGE_USERNAME`, `SOLAREDGE_PASSWORD`, and `SOLAREDGE_API_KEY` are **all
  required env vars**. Bail at startup with a clear error if any is missing.
- `config.toml` is **static** (site_id + field → serial mappings). It is
  `.gitignore`d and baked into the Docker image at build time (`COPY config.toml`
  in the Dockerfile). Don't add code paths that expect it to be volume-mounted,
  hot-reloaded, or pulled from env.
- The logical layout (inverter → optimizer tree) is fetched **once at startup**
  and never refreshed. The physical PV install is static; if it changes, the
  user restarts the process. Don't add a periodic layout refresh.
- Telemetry refreshes every `refresh.optimizer_seconds` (default 900 s, matching
  the portal's own update cadence). Polling faster is pointless.
- The public Monitoring API task refreshes every
  `monitoring_api.refresh_seconds` (default 1800 s). Don't drop below 900 s
  without recomputing the 300 req/day budget — three endpoints per cycle ×
  96 cycles/day = 288 calls/day, leaving almost no headroom for retries.
  `solaredge_monitoring_api_requests_total` exposes the budget live.
- `monitoring_api.state_file` (optional) persists the AC-grid-charging
  counter across restarts. Written atomically (tempfile + rename) inside
  `MonitoringApiClient::persist_state()` after every successful storage
  fetch. When unset, the counter resets on restart and startup logs a WARN.
  In Docker, mount a volume over the parent directory. Don't quietly
  enable by default — that would imply write access to the container
  filesystem, which breaks the "stateless by default" story.

## Portal endpoints (undocumented, `src/portal/`)

**The legacy portal API is gone.** On ~2026-07-21 SolarEdge retired
`GET /solaredge-apigw/api/sites/{siteId}/layout/logical` — it returns **HTTP 410
Gone** with a body containing only
`https://marketing.solaredge.com/legacy-api-sign-up-for-interest`. The 410 is
served at the CDN edge *before* auth, so it is not a credentials problem. The old
`/solaredge-web/p/login` page now 301s to the `/one` SPA. Everything in
`src/portal/` has therefore moved to the **SolarEdge ONE `/services/` platform**.

All endpoints are on `monitoring.solaredge.com` and **all** use the Cognito
access token (bearer header + `se_monitoring_auth` cookie; we send both):

| Endpoint | Purpose |
| --- | --- |
| `GET /services/layout/logical/generic/v2/site/{siteId}?include-optimizers=true` | Logical layout tree (replaces `layout/logical`) |
| `POST /services/layout/information/optimizers` — body is a JSON array of serials | Live per-optimizer telemetry for the **whole site in one call** (replaces per-optimizer `systemData`) |
| `GET /services/layout/energy-graph/site/{siteId}/optimizers?chart-time-unit=years&start-date=…&end-date=…&optimizer-serials=…` | Per-optimizer lifetime energy (replaces `layout/energy`) |
| `GET /services/dashboard/energy/sites/{siteId}?chart-time-unit=years&start-date=…&end-date=…&measurement-types=production-distribution-with-storage,consumption-distribution-with-storage&isCniViewer=true` | Site battery charge/discharge energy (unchanged) |

Notes on each:

- **Layout tree** nodes are self-describing via a `type` field
  (`INVERTER`/`STRING`/`OPTIMIZER`, plus `FOLDER` nodes that only group
  same-typed siblings). Real shape is folder-wrapped at every level:
  `FOLDER(INVERTER) > INVERTER > FOLDER(STRING) > STRING > FOLDER(OPTIMIZER) > OPTIMIZER`.
  `LayoutNodeV2::typed_children` descends through the folders and also accepts
  the unwrapped shape. The response may or may not be wrapped in a
  `siteStructure` envelope — `LayoutNodeV2::root()` handles both.
- **There is no `reporter_id` any more.** The ONE API is keyed by optimizer
  **serial**, which is what `config.toml`'s `[[fields]]` already lists. Serial
  precedence when reading a node: `serial` → `properties.identifier` → `uuid`.
- **Live telemetry** returns `serialToLiveData` keyed by serial, with
  `power_W`, `current_A`, `voltage_V`, `optimizerVoltage_V`, `lastMeasurement`.
  These map 1:1 onto the six existing gauges, so no metric or label changed.
  An optimizer that is in the layout but `REPLACED`/inactive has **no entry** —
  `FlatOptimizer::is_active()` keeps that at DEBUG instead of warning forever.
- **`lastMeasurement` is ISO 8601** here; the retired endpoint emitted
  `"Thu Apr 23 12:26:12 GMT 2026"`. `parse_last_measurement` tries ISO first and
  keeps the legacy branch as a fallback. Getting this wrong silently drops the
  `last_measurement` gauge (the old parser returned `None` for ISO).
- **Energy-graph `totalEnergy` is a single scalar for all serials passed**, so it
  cannot be batched without silently summing the site — it is queried one
  optimizer at a time, fanned out `ENERGY_CONCURRENCY` at a time in `scrape.rs`.
  Wide fixed windows (`OPTIMIZER_ENERGY_START_DATE` / `BATTERY_ENERGY_START_DATE`)
  keep the cumulative totals monotonic.
- Responses are clean JSON. The old junk-tolerant `extract_json` scanner is gone
  with the `systemData` endpoint that needed it.

**Auth is now uniform: everything is Cognito.** The `/services/` platform is
gated by an **AWS Cognito** access-token JWT (user pool
`eu-central-1_fVUTz39em`, app client `ugfnsujd3384sshcjehaphlh3`).
`USER_PASSWORD_AUTH` is disabled, so we log in via **SRP** (`USER_SRP_AUTH`)
using the same `SOLAREDGE_USERNAME` / `SOLAREDGE_PASSWORD` —
`portal/cognito.rs` runs the handshake (crypto by `aws-cognito-srp`, HTTP by us
against `cognito-idp.eu-central-1.amazonaws.com`), and
`PortalClient::ensure_se_monitoring_auth` caches the token (~24 h), seeds the
cookie jar, and returns the token for the bearer header. HTTP Basic auth,
`login()`, and the CSRF-token dance are all gone — there is no Spring session
left to warm. `solaredge_portal_login_total` now counts real SRP handshakes
(~1/day). If SolarEdge rotates the pool/client IDs, re-capture from a browser
login and update the constants in `portal/cognito.rs`.

A layout fetch failure is **not fatal** — see "Refresh model" below.

## Public Monitoring API endpoints (`src/monitoring_api/`)

All on `monitoringapi.solaredge.com`, all take `?api_key={key}` query param:

| Endpoint | Purpose |
| --- | --- |
| `GET /site/{siteId}/overview` | Site PV lifetime energy (`overview.lifeTimeData.energy`) |
| `GET /site/{siteId}/meters?meters=Production,Consumption,FeedIn,Purchased&startTime&endTime&timeUnit=DAY` | Per-meter lifetime energy — we take the most recent `value` |
| `GET /site/{siteId}/storageData?startTime&endTime` | Per-battery telemetry list — we take the latest entry for `ACGridCharging`, `fullPackEnergyAvailable`, power/temp/state. **Not** charge/discharge energy — those `lifeTimeEnergy*` fields are 0 here; see portal endpoint above |

Response field `unscaledEnergy` (not used here, but in the portal energy
endpoint) can arrive as either a number or a quoted string. Storage endpoint
window is capped at 7 days.

## If the portal API breaks

Diff against `AndrewTapp/solaredgeoptimizers` — the **active** fork, which
implements the same `/services/` endpoints we target. README.md pins the exact
commit SHA. One-liner:

```sh
git -C /tmp clone https://github.com/AndrewTapp/solaredgeoptimizers
git -C /tmp/solaredgeoptimizers \
    diff <PINNED_SHA> HEAD -- custom_components/solaredgeoptimizers/solaredge_one_api.py
```

When you update the Rust port to match an upstream change, **bump the pinned
SHA in `README.md`** so future diffs stay scoped.

`ProudElm/packaging_solaredgeoptimizers`, the original port source, has been
dormant since 2023 and was never fixed for the July 2026 shutdown — don't diff
against it.

Useful triage trick: the 410 is served before auth, so `curl -o /dev/null -w
'%{http_code}'` against an endpoint distinguishes "retired" (410) from "alive but
needs auth" (401) without credentials. A 404 means the route doesn't exist at all;
note that `/services/{dashboard,layout,alerts,charts,cni}/**` are auth-gated
*prefixes*, so any path under them answers 401 whether or not the specific route
is real.

## Bootstrapping `config.toml`

On startup the exporter fetches the layout once and logs every optimizer
(inverter serial, optimizer serial, display name, layout status) at `INFO`. A
user with an empty `[[fields]]` list runs it once, copies serials from the
log, restarts.

Optimizers not listed in any field get `field="unassigned"`; nothing is
silently dropped.

## Commands

```sh
cargo fmt --check               # matches lefthook + CI
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release

# local smoke run
SOLAREDGE_USERNAME=… SOLAREDGE_PASSWORD=… SOLAREDGE_API_KEY=… \
  cargo run -- --config config.toml

# docker
docker build -t solaredge_exporter .
docker run --rm \
  -e SOLAREDGE_USERNAME=… -e SOLAREDGE_PASSWORD=… -e SOLAREDGE_API_KEY=… \
  -p 8888:8888 solaredge_exporter
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo build`,
`cargo test`. Lefthook runs fmt + clippy pre-commit.

## Refresh model (don't change without asking)

The exporter refreshes in a **background loop, not on /metrics scrape**. Reasons:

- Prometheus scrape timeout is 10 s by default; our refresh is ~10 s and will
  slow down if the portal does. Scrape-triggered would mark the target down.
- Multiple concurrent scrapes (HA Prometheus pair, Grafana Explore, etc.)
  would fire multiple portal fetches without a mutex + cache.
- The portal itself only updates every 15 min — scraping faster is pointless.

To avoid partial reads, `refresh_once` is structured as two phases:

1. **Gather** (async, ~10 s): warm the Cognito token, then one batch call for all
   optimizer telemetry, one energy call per optimizer (fanned out
   `ENERGY_CONCURRENCY` at a time), and the battery-energy call. Buffer readings
   in a `Vec<(OptimizerLabels, OptimizerReading)>` — no gauge writes.
2. **Commit** (synchronous, microseconds): flush all buffered readings to
   gauges in one tight loop with no awaits.

This keeps the inconsistent-read window under a millisecond. If you ever need
truly atomic (byte-level) reads, wrap the `AppMetrics` families behind an
`ArcSwap` or `tokio::sync::RwLock` — but don't switch to scrape-triggered.

**Never hold two `Family::get_or_create` guards at once.** It returns a guard
holding a *read* lock on the family's label map, and creating a new label set
needs the *write* lock — so binding two of them deadlocks. Every call site keeps
the guard to a statement-local temporary that drops at the semicolon; keep it
that way (`lifetime_clamp_is_per_label_set` in `metrics.rs` documents the trap).

**A layout fetch failure is not fatal.** Nothing outside the optimizer metrics
depends on the layout, so `main` logs the error, bumps
`solaredge_refresh_errors_total{kind="layout"}`, and continues with an empty
optimizer list — the HTTP server and the public-API task (battery/meter/site-PV)
still come up. This exists because the July 2026 `layout/logical` shutdown
aborted startup outright and took every unrelated metric down with it. Don't
reintroduce a `?` there. Note `AppMetrics::new()` must stay *above* the layout
fetch so the error counter exists when it fails.

**Prometheus `scrape_interval` must stay well under 5 min (60s recommended),
even though upstream only refreshes every 15–30 min.** `/metrics` serves cached
gauges (no upstream calls), so frequent scrapes are free. A 15 min interval
exceeds Prometheus's 5 min instant-query lookback, so every series goes "stale"
between scrapes and disappears from instant queries, dashboards, and alert-rule
evaluations (the target still shows healthy — only the stored samples are too
sparse). Alert rules in `monitoring/solaredge.rules.yml` use `*_over_time([20m])`
windows to tolerate a too-long interval, but don't rely on that. Staleness
alerting keys off `solaredge_*_last_refresh_timestamp_seconds`, which the
exporter stamps per source on each successful refresh — including
`kind="battery_energy"`, stamped only when charge/discharge values were actually
extracted so silent schema drift on the unofficial `/services/` endpoint trips
the alert.

## Out of scope (don't add without asking)

- Additional public Monitoring API endpoints beyond `overview`, `meters`,
  `storageData` — each extra endpoint eats into the 300 req/day cap
- Chart / historical data endpoints (`chartData`, `requestItemHistory`)
- Site-level metrics already provided by the modbus exporter (inverter AC power,
  meter import/export, battery SoC, etc.)
- Layout hot-reload, config hot-reload
- CLI subcommands beyond the single `--config` flag
- Multi-site support (config is single `site_id`)
