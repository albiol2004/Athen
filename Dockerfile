# Athen headless daemon image.
#
# The binary is the same athen-app the desktop uses, started with
# --headless: full autonomous stack (senses, coordinator, dispatch,
# wake-ups), Telegram as the user surface, no GUI initialized. The
# WebKitGTK/GTK libs are present only because Tauri's types are linked
# into the binary; they are never initialized at runtime. Shrinking the
# image by extracting a Tauri-free runtime crate is future work.
#
# Per-instance model: one container = one user/instance. Data lives in
# the /data volume (ATHEN_DATA_DIR); secrets come in as env vars or
# *_FILE-mounted secrets (see docs/HEADLESS.md).
#
#   docker build -t athen .
#   docker run -d -v athen-data:/data \
#     -e ATHEN_TELEGRAM_BOT_TOKEN=... \
#     -e ATHEN_PROVIDER_DEEPSEEK_API_KEY=... \
#     athen

# trixie: the prebuilt onnxruntime (ort_sys, via bundled embeddings)
# requires glibc >= 2.38 (__isoc23_* symbols); bookworm's 2.36 fails to link.
FROM rust:1-trixie AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    libwebkit2gtk-4.1-dev \
    libgtk-3-dev \
    libsoup-3.0-dev \
    libayatana-appindicator3-dev \
    librsvg2-dev \
    pkg-config \
    cmake \
    clang \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Nushell sidecar: tauri-build validates externalBin existence at compile
# time, and the agent shell prefers the embedded nu over the native shell.
RUN bash scripts/fetch-nushell.sh

RUN cargo build --release -p athen-app -p athen-cli

FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    libwebkit2gtk-4.1-0 \
    libayatana-appindicator3-1 \
    ca-certificates \
    git \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 athen \
    && mkdir -p /data && chown athen:athen /data

COPY --from=build /src/target/release/athen-app /usr/local/bin/athen-app
COPY --from=build /src/target/release/athen-cli /usr/local/bin/athen-cli
# The shell layer looks for the bundled nu next to the app binary.
COPY --from=build /src/crates/athen-app/binaries/nu-x86_64-unknown-linux-gnu /usr/local/bin/nu

USER athen
ENV ATHEN_DATA_DIR=/data \
    ATHEN_VAULT_BACKEND=file \
    ATHEN_HEADLESS=1 \
    RUST_LOG=info

VOLUME /data

ENTRYPOINT ["/usr/local/bin/athen-app"]
CMD ["--headless"]
