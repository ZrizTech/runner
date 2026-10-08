# zriz-runner: runs inside the customer's environment, executes ops the
# zriz cloud sends, holds every secret. Never imports from the cloud.
# Base images are pinned by digest (multi-arch index). Update them with a new patch release.
FROM rust:1-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS build
WORKDIR /src
COPY . .
ARG ZRIZ_BUILD_SHA=""
ENV ZRIZ_BUILD_SHA=$ZRIZ_BUILD_SHA
RUN cargo build --release --bin zriz-runner && \
    strip target/release/zriz-runner

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/target/release/zriz-runner /zriz-runner
ENTRYPOINT ["/zriz-runner"]
