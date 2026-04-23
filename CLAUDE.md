# CLAUDE.md

Project-specific guidance for Claude Code when working in this repository.

## What this is

Rust Prometheus exporter for SolarEdge **per-optimizer** metrics. Complements a
separate modbus-based exporter (site/inverter/meter/battery level) — do not
duplicate those metrics here. Implementation ports the Python library
`ProudElm/packaging_solaredgeoptimizers` and scrapes the undocumented
`monitoring.solaredge.com` portal API. The public SolarEdge Monitoring API is
**not used** (no per-optimizer data, 300 req/day cap) and should not be added
without an explicit ask.

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

- `SOLAREDGE_USERNAME` and `SOLAREDGE_PASSWORD` are **required env vars**. Bail
  at startup with a clear error if missing.
- `config.toml` is **static** (site_id + field → serial mappings). It is
  `.gitignore`d and baked into the Docker image at build time (`COPY config.toml`
  in the Dockerfile). Don't add code paths that expect it to be volume-mounted,
  hot-reloaded, or pulled from env.
- The logical layout (inverter → optimizer tree) is fetched **once at startup**
  and never refreshed. The physical PV install is static; if it changes, the
  user restarts the process. Don't add a periodic layout refresh.
- Telemetry refreshes every `refresh.optimizer_seconds` (default 900 s, matching
  the portal's own update cadence). Polling faster is pointless.

## Portal endpoints

| Endpoint | Auth |
| --- | --- |
| `GET monitoring.solaredge.com/solaredge-web/p/login` | Basic |
| `GET monitoring.solaredge.com/solaredge-apigw/api/sites/{siteId}/layout/logical` | Basic |
| `GET monitoringpublic.solaredge.com/solaredge-web/p/publicSystemData?reporterId=…&type=panel&fieldId={siteId}` | Basic |
| `POST monitoring.solaredge.com/solaredge-apigw/api/sites/{siteId}/layout/energy?timeUnit=ALL` | Basic + CSRF cookie |

`publicSystemData` responses may have non-JSON prefix junk — use
`client::extract_json` (mirrors Python's `jsonfinder`).

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
SOLAREDGE_USERNAME=… SOLAREDGE_PASSWORD=… \
  cargo run -- --config config.toml

# docker
docker build -t solaredge_exporter .
docker run --rm -e SOLAREDGE_USERNAME=… -e SOLAREDGE_PASSWORD=… \
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

- Public SolarEdge Monitoring API calls
- Chart / historical data endpoints (`chartData`, `requestItemHistory`)
- Site-level metrics already provided by the modbus exporter (inverter AC power,
  meter import/export, battery SoC, etc.)
- Layout hot-reload, config hot-reload
- CLI subcommands beyond the single `--config` flag
- Multi-site support (config is single `site_id`)
