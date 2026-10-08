# zriz-worker: Node sidecar next to the runner. Runs browser and cli ops. Holds no runner secret.
# Build context is the repo root (the worker reads ../worker-contract/ at start):
#   docker buildx build -f worker/Dockerfile .
# Base: node:22-bookworm-slim pinned by digest (multi-arch index). Chromium only, via the pinned Playwright.
FROM node:22-bookworm-slim@sha256:43ac6c60b8f89723f746e8a92ce91abd5017e627ce1ddfe4238355d3a30b772c

ENV NODE_ENV=production \
    PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright \
    ZRIZ_WORKER_SOCKET=/run/zriz/worker.sock

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    npx --yes playwright@1.63.0 install --with-deps chromium && \
    chmod -R a+rX /opt/ms-playwright && \
    rm -rf /var/lib/apt/lists/* /root/.npm

# Same layout as the repo: src reaches ../../worker-contract from /opt/zriz/worker/src.
WORKDIR /opt/zriz/worker
COPY worker/package.json worker/package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force
COPY worker/src ./src
COPY worker-contract /opt/zriz/worker-contract

USER 65532:65532
CMD ["node", "src/index.js"]
