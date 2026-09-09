FROM rust:1-bookworm AS rust-chef
WORKDIR /src
# Pin the toolchain before any compile so `chef cook` and `cargo build` use the
# same rustc. Without this, cook compiles deps with the image's bundled
# toolchain, then `COPY . .` surfaces rust-toolchain.toml and cargo build
# switches compilers — invalidating the entire cooked dependency cache.
COPY rust-toolchain.toml ./
RUN rustup toolchain install
RUN cargo install --locked --version 0.1.73 cargo-chef

FROM rust-chef AS rust-planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM rust-chef AS rust-build
COPY --from=rust-planner /src/recipe.json recipe.json
RUN cargo chef cook --locked --release --workspace --recipe-path recipe.json
COPY . .
RUN cargo build --locked --release --bin blue --bin control-api --bin inference-proxy \
    && mkdir -p /out \
    && cp target/release/blue target/release/control-api target/release/inference-proxy /out/

# A minimal cross-build context for consumers that only need the compiled
# binaries. Keeping the Rust toolchain and target cache out of this image avoids
# exhausting Docker storage when Compose materializes service contexts.
FROM scratch AS rust-artifacts
COPY --from=rust-build /out/ /out/

FROM node:22-bookworm-slim AS dashboard-build
ARG BLUE_DEPLOYMENT_VERSION=development
ENV BLUE_DEPLOYMENT_VERSION=$BLUE_DEPLOYMENT_VERSION
WORKDIR /src/apps/dashboard
COPY apps/dashboard/package.json apps/dashboard/package-lock.json ./
RUN npm ci
COPY apps/dashboard/ ./
RUN npm run build

FROM node:22-bookworm-slim
ARG BLUE_DEPLOYMENT_VERSION=development
ENV NODE_ENV=production \
    BLUE_DEPLOYMENT_VERSION=$BLUE_DEPLOYMENT_VERSION \
    BLUE_CONFIG_FILE=/etc/blue/blue.yaml
# Keep only runtime tooling: git supports managed package repositories and tini
# provides signal-correct process supervision. npm is build-only and removing it
# also removes an unnecessary package-management attack surface.
RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends ca-certificates git tini \
    && rm -rf /usr/local/lib/node_modules/npm /usr/local/bin/npm /usr/local/bin/npx \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust-build /out/control-api /usr/local/bin/control-api
COPY --from=rust-build /out/inference-proxy /usr/local/bin/inference-proxy
COPY --from=dashboard-build /src/apps/dashboard/.next/standalone /opt/blue/dashboard
COPY --from=dashboard-build /src/apps/dashboard/.next/static /opt/blue/dashboard/.next/static
COPY --from=dashboard-build /src/apps/dashboard/public /opt/blue/dashboard/public
COPY --from=dashboard-build /src/apps/dashboard/dashboard-start.mjs /opt/blue/dashboard/dashboard-start.mjs
COPY --from=dashboard-build /src/apps/dashboard/lib/runtime-config.mjs /opt/blue/dashboard/lib/runtime-config.mjs
COPY deploy/blue-entrypoint.sh /usr/local/bin/blue-entrypoint
COPY deploy/blue-healthcheck.sh /usr/local/bin/blue-healthcheck
RUN chmod 0755 /usr/local/bin/blue-entrypoint /usr/local/bin/blue-healthcheck \
    && mkdir -p /etc/blue \
    && chown -R node:node /opt/blue /etc/blue
USER node
EXPOSE 3000 8080 8081 8082
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 CMD ["/usr/local/bin/blue-healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/blue-entrypoint"]
CMD ["all"]
