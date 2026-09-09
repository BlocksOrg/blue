ARG BLUE_BASE_IMAGE=blue-e2e-base:local
FROM ${BLUE_BASE_IMAGE}
COPY --chmod=0555 tests/e2e/fixtures/custom-provisioner/provisioner.sh /etc/blue/e2e-provisioner
