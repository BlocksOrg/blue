ARG BLUE_E2E_RUST_IMAGE=blue-e2e-rust:local
FROM ${BLUE_E2E_RUST_IMAGE} AS cli-build

FROM mcr.microsoft.com/playwright:v1.55.0-noble
WORKDIR /work/tests/e2e
COPY --from=cli-build /out/blue /usr/local/bin/blue
COPY tests/e2e/package.json tests/e2e/package-lock.json ./
RUN npm ci
COPY tests/e2e/ ./
COPY deploy/contract/governance.openapi.yaml fixtures/governance.openapi.yaml
RUN chmod 0755 fixtures/bin/fake-agent run-in-container.sh
RUN for agent in codex claude kimi opencode; do ln -s /work/tests/e2e/fixtures/bin/fake-agent /usr/local/bin/$agent; done
ENV E2E_DASHBOARD_URL=http://127.0.0.1:3000 \
    E2E_CONTROL_API_URL=http://127.0.0.1:8080 \
    E2E_STATE_ROOT=/tmp/blue-e2e
CMD ["./run-in-container.sh", "smoke"]
