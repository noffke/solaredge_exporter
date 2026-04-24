# CLAUDE.md

Project-specific guidance for Claude Code when working in this repository.

## What this is

Rust Prometheus exporter combining two SolarEdge data sources:

1. **Undocumented portal scrape** (`src/portal/`) — per-optimizer live
   telemetry. Ports `ProudElm/packaging_solaredgeoptimizers` against
   `monitoring.solaredge.com`. No request budget; refreshes every 15 min.
2. **Public Monitoring API** (`src/monitoring_api/`) — battery lifetime
   counters (`lifeTimeEnergyCharged`, `lifeTimeEnergyDischarged`,
   `ACGridCharging`, `fullPackEnergyAvailable`, SoC/power/temp/state),
   site meter lifetime energy, and site PV lifetime energy. Hand-rolled
   against `monitoringapi.solaredge.com` (we don't use the `solaredge`
   crate — its `http-adapter` transitively pins `reqwest 0.12` and we're
   on 0.13). **Hard-capped at 300 req/day**; refreshes every 30 min with
   three calls per cycle (~144 calls/day).

Complements a separate modbus-based exporter (inverter/meter/DER live power,
battery SoC). Don't duplicate those metrics here — add new ones via the public
API only when they plug a genuine gap (e.g. battery charge/discharge lifetime).

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

Prefer these when extending the code. Don't introduce `chrono`, `log`, `hyper`
directly, or `native-tls`.

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

| Endpoint | Auth |
| --- | --- |
| `GET monitoring.solaredge.com/solaredge-web/p/login` | Basic |
| `GET monitoring.solaredge.com/solaredge-apigw/api/sites/{siteId}/layout/logical` | Basic |
| `GET monitoring.solaredge.com/solaredge-web/p/systemData?reporterId=…&type=panel&fieldId={siteId}&isPublic=false&locale=en_US&v={millis}` | Basic |
| `POST monitoring.solaredge.com/solaredge-apigw/api/sites/{siteId}/layout/energy?timeUnit=ALL` | Basic + CSRF cookie + `Content-Type: application/json` |

`systemData` responses have non-JSON prefix junk — use `client::extract_json`
(mirrors Python's `jsonfinder`).

## Public Monitoring API endpoints (`src/monitoring_api/`)

All on `monitoringapi.solaredge.com`, all take `?api_key={key}` query param:

| Endpoint | Purpose |
| --- | --- |
| `GET /site/{siteId}/overview` | Site PV lifetime energy (`overview.lifeTimeData.energy`) |
| `GET /site/{siteId}/meters?meters=Production,Consumption,FeedIn,Purchased&startTime&endTime&timeUnit=DAY` | Per-meter lifetime energy — we take the most recent `value` |
| `GET /site/{siteId}/storageData?startTime&endTime` | Per-battery telemetry list — we take the latest telemetry entry |

Response field `unscaledEnergy` (not used here, but in the portal energy
endpoint) can arrive as either a number or a quoted string. Storage endpoint
window is capped at 7 days.

## If the portal API breaks

Diff against the Python source we ported from. README.md pins the exact commit
SHA. One-liner:

```sh
git -C /tmp clone https://github.com/ProudElm/packaging_solaredgeoptimizers
git -C /tmp/packaging_solaredgeoptimizers \
    diff <PINNED_SHA> HEAD -- src/solaredgeoptimizers/solaredgeoptimizers.py
```

When you update the Rust port to match an upstream change, **bump the pinned
SHA in `README.md`** so future diffs stay scoped.

## Bootstrapping `config.toml`

On startup the exporter fetches the layout once and logs every optimizer
(inverter serial, optimizer serial, display name, reporter ID) at `INFO`. A
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

1. **Gather** (async, ~10 s): fetch every optimizer + energy, buffer readings
   in a `Vec<(OptimizerLabels, OptimizerReading)>` — no gauge writes.
2. **Commit** (synchronous, microseconds): flush all buffered readings to
   gauges in one tight loop with no awaits.

This keeps the inconsistent-read window under a millisecond. If you ever need
truly atomic (byte-level) reads, wrap the `AppMetrics` families behind an
`ArcSwap` or `tokio::sync::RwLock` — but don't switch to scrape-triggered.

## Out of scope (don't add without asking)

- Additional public Monitoring API endpoints beyond `overview`, `meters`,
  `storageData` — each extra endpoint eats into the 300 req/day cap
- Chart / historical data endpoints (`chartData`, `requestItemHistory`)
- Site-level metrics already provided by the modbus exporter (inverter AC power,
  meter import/export, battery SoC, etc.)
- Layout hot-reload, config hot-reload
- CLI subcommands beyond the single `--config` flag
- Multi-site support (config is single `site_id`)
