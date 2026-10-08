# zriz-runner: runs inside the customer's environment, executes ops the
# zriz cloud sends, holds every secret. Never imports from the cloud.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
ARG ZRIZ_BUILD_SHA=""
ENV ZRIZ_BUILD_SHA=$ZRIZ_BUILD_SHA
RUN cargo build --release --bin zriz-runner && \
    strip target/release/zriz-runner

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/zriz-runner /zriz-runner
ENTRYPOINT ["/zriz-runner"]
