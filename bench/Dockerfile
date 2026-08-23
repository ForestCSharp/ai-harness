# The ai-harness binary, built once and mounted into task containers.
#
# Deliberately not a runtime image for the benchmarks themselves: every one of
# the three brings its own per-task container with the repository under test
# already in it. What they need from us is one static-ish binary to bind-mount,
# and building it per instance would dominate the wall-clock figure they report.
#
# Build:  docker build -f bench/Dockerfile -t ai-harness:bench .
# Export: docker create --name x ai-harness:bench && \
#         docker cp x:/opt/ai-harness/ai-harness ./target/release/ai-harness

FROM rust:1-slim-bookworm AS build

# The slim image has no C toolchain, and rustls' crypto backend (ring or
# aws-lc-rs depending on how the tree resolves) builds native code. Without
# these the dependency layer fails on a linker that is not there — which is a
# confusing way to find out, several minutes in.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential cmake perl pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Manifests first, so a source-only change does not re-download the index.
COPY Cargo.toml Cargo.lock ./
# A stub main lets the dependency layer build and cache on its own.
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Cargo times the stub by the mtime it recorded; touching the real entry point
# is what stops it deciding nothing changed and shipping the stub binary.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim

# The harness shells out to /bin/sh, and reaches the network for OpenRouter and
# for <ai-harness-fetch>; ca-certificates is what makes the TLS work.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/ai-harness /opt/ai-harness/ai-harness
ENV PATH="/opt/ai-harness:${PATH}"

# No sandbox on Linux: Seatbelt is macOS-only and the harness refuses to run
# commands unconfined unless told that something else is the boundary. In a
# per-task container that is true, which is why `--sandbox=none` exists and why
# it is accepted only alongside `--headless`.
ENTRYPOINT ["ai-harness", "--headless", "--sandbox", "none"]
CMD ["--help"]
