# zriz-runner: runs inside the customer's environment, executes ops the
# zriz cloud sends, holds every secret. Never imports from the cloud.
# Base images are pinned by digest (multi-arch index). Update them with a new patch release.
# The build stage runs on the build host and cross-compiles: no QEMU, no slow arm64 compile.
FROM --platform=$BUILDPLATFORM rust:1-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS build
# cargo-auditable puts the crate list into the binary, so the SBOM of the final image lists the crates.
# (BUILDKIT_SBOM_SCAN_STAGE did not list them: the scanner skips Cargo.lock.)
RUN cargo install cargo-auditable --version 0.7.4 --locked
ARG BUILDARCH
ARG TARGETARCH
WORKDIR /src
COPY . .
ARG ZRIZ_BUILD_SHA=""
ENV ZRIZ_BUILD_SHA=$ZRIZ_BUILD_SHA
# Another target arch than the build host: install a cross C compiler (ring needs one) and use it as linker.
RUN set -eu; \
    case "$TARGETARCH" in \
      arm64) triple=aarch64-unknown-linux-gnu; pkg=gcc-aarch64-linux-gnu; cc=aarch64-linux-gnu-gcc; up=AARCH64 ;; \
      *) triple=x86_64-unknown-linux-gnu; pkg=gcc-x86-64-linux-gnu; cc=x86_64-linux-gnu-gcc; up=X86_64 ;; \
    esac; \
    strip=strip; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update && apt-get install -y --no-install-recommends "$pkg" "libc6-dev-$TARGETARCH-cross" && rm -rf /var/lib/apt/lists/*; \
      export "CARGO_TARGET_${up}_UNKNOWN_LINUX_GNU_LINKER=$cc" "CC_$(echo "$triple" | tr - _)=$cc"; \
      strip="${cc%gcc}strip"; \
    fi; \
    rustup target add "$triple"; \
    cargo auditable build --locked --release --target "$triple" --bin zriz-runner; \
    "$strip" "target/$triple/release/zriz-runner"; \
    cp "target/$triple/release/zriz-runner" /zriz-runner

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /zriz-runner /zriz-runner
ENTRYPOINT ["/zriz-runner"]
