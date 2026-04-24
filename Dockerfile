FROM rust:1.95.0 AS builder

COPY . .
RUN cargo build --release

FROM ubuntu:noble
ENV TZ=Europe/Berlin
ENV RUST_LOG=info
EXPOSE 8888

RUN apt-get update && \
DEBIAN_FRONTEND=noninteractive apt-get -y install --no-install-recommends tzdata ca-certificates && \
ln -fs /usr/share/zoneinfo/${TZ} /etc/localtime && \
dpkg-reconfigure --frontend noninteractive tzdata && \
rm -rf /var/lib/apt/lists/*

COPY --from=builder target/release/solaredge_exporter .
COPY config.toml config.toml

# If `monitoring_api.state_file` is configured (e.g. "/state/state.json"),
# mount a volume over the parent directory at `docker run` time so the AC
# grid-charging counter survives container restarts:
#     docker run -v solaredge_state:/state … solaredge_exporter

ENTRYPOINT ["./solaredge_exporter", "--config", "config.toml"]