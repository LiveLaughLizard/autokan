# syntax=docker/dockerfile:1

# ---- build ---------------------------------------------------------------------------------
# Debian 12 on both stages, so the C libraries collected below match the runtime image.
FROM rust:1-bookworm AS build

# libxml2 + xmlsec1 are what SAML signature verification links against; clang is for bindgen.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config clang libclang-dev libxml2-dev libxmlsec1-dev libxmlsec1-openssl libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# static/ (built CSS and vendored JS) is committed and compiled into the binary, so no Node stage is needed.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/autokan /autokan

# The runtime image has no package manager, so gather the shared libraries the binary loads
# (libxml2, xmlsec1 and what they depend on) into one directory to copy across. The C runtime
# itself is left out: the distroless image already provides it.
RUN mkdir -p /runtime/libs /runtime/data \
 && ldd /autokan | awk '$3 ~ /^\// { print $3 }' \
    | grep -Ev '/(libc|libm|libdl|libpthread|librt|libresolv|libgcc_s|libstdc\+\+)\.so' \
    | xargs -I{} cp -L {} /runtime/libs/ \
 && /autokan --version

# ---- runtime -------------------------------------------------------------------------------
# Distroless: glibc, CA certificates and a non-root user; no shell, no package manager.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=build /runtime/libs/ /usr/local/lib/autokan/
COPY --from=build /autokan /usr/local/bin/autokan
# An empty, writable data directory for the SQLite database and uploads (mount a volume here).
COPY --from=build --chown=nonroot:nonroot /runtime/data /data

ENV LD_LIBRARY_PATH=/usr/local/lib/autokan \
    AUTOKAN_BIND=0.0.0.0:3000 \
    AUTOKAN_DATA_DIR=/data
EXPOSE 3000
VOLUME ["/data"]
USER nonroot
HEALTHCHECK --interval=30s --timeout=6s --start-period=20s CMD ["/usr/local/bin/autokan", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/autokan"]
