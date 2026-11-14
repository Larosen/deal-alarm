# --- Build ---
FROM rust:1-bookworm AS build
WORKDIR /src
# Abhängigkeiten zuerst bauen, damit sie bei Code-Änderungen im Cache bleiben
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release && rm -rf src target/release/deps/deal_alarm* target/release/deal-alarm*
COPY src ./src
COPY assets ./assets
RUN cargo build --release

# --- Laufzeit ---
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="Deal-Alarm" \
      org.opencontainers.image.description="Dealalarm für expert, Euronics & Amazon: Preise prüfen, Benachrichtigung per ntfy" \
      org.opencontainers.image.source="https://github.com/Larosen/deal-alarm"

ENV DATA_DIR=/data \
    PORT=8080 \
    TZ=Europe/Berlin

# Zeitzonendaten, sonst greift TZ nicht und alle Zeiten laufen in UTC
RUN apt-get update \
    && apt-get install -y --no-install-recommends tzdata \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/deal-alarm /usr/local/bin/deal-alarm

VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=60s --timeout=5s --start-period=20s \
    CMD ["deal-alarm", "--healthcheck"]
CMD ["deal-alarm"]
