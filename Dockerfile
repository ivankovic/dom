# Builds the `dom` binary for running multiple instances side by side under
# Docker Compose (see compose.yaml) — a stand-in for two separate machines
# when testing the cluster code (SPECS.md, "High availability: a two-node
# active/standby cluster") without two Raspberry Pis on hand. Not the
# platform Dom is meant to run on day to day — see README.md, "Small
# machines" — this is a testing convenience, not a deployment target.

FROM rust:1-bookworm AS builder
WORKDIR /build

# `libsqlite3-sys` (via sqlx's "sqlite" feature) and `ring` each need a C
# compiler to build their vendored source — see Cargo.toml's comment on why
# `ring` was chosen over the default crypto provider precisely because it
# needs no cmake/nasm on top of this.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Only what a release build of the binary needs — not `tests/`, which is
# unused here and would just slow down every rebuild.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin dom

# A plain Debian base rather than `scratch`/distroless: Dom's own README
# documents it as Linux-only software that expects a normal userland (it
# shells out to nothing, but this keeps the image easy to `docker exec` into
# for debugging a test run). No `ca-certificates` package: the two HTTPS
# services Dom talks to (`src/online/https.rs`) validate against the
# `webpki-roots` bundle compiled into the binary, not the system store.
FROM debian:bookworm-slim
COPY --from=builder /build/target/release/dom /usr/local/bin/dom

# `db.sqlite` and `dom.log` are always written relative to the working
# directory (`src/main.rs`, `src/logging.rs`) — this is what makes a bind- or
# named-volume mount at `/data` sufficient for per-instance isolation, with
# no environment variable or flag needed to tell Dom where to put anything.
WORKDIR /data
ENTRYPOINT ["/usr/local/bin/dom"]
