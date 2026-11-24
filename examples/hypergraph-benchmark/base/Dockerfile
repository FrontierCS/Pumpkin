# What the agent and the judge share: the system, the read-only starter and the edit scope.
FROM python:3.12-slim-bookworm@sha256:935c3c5b12357a7e10caa9c1e6ca5692344d97cf229832ef584be2cec9fc03df AS certificates

FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
# The certificates reach the dated snapshot that every package comes from over HTTPS.
COPY --from=certificates /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ARG DEBIAN_FRONTEND=noninteractive
RUN printf 'Types: deb\nURIs: https://snapshot.ubuntu.com/ubuntu/20261103T000000Z\nSuites: noble noble-updates noble-backports noble-security\nComponents: main restricted universe multiverse\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n' \
       > /etc/apt/sources.list.d/ubuntu.sources \
    && printf 'Acquire::Retries "10";\nAcquire::Retries::Delay::Maximum "60";\n' > /etc/apt/apt.conf.d/80-retries \
    && apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates git python3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ligra /LICENSE /opt/starter/LICENSE
COPY --from=ligra /ligra /opt/starter/ligra
COPY --from=ligra /apps/hyper/Makefile /opt/starter/apps/hyper/Makefile
COPY repo /opt/starter
COPY edit_scope.py /opt/team/
RUN find /opt/starter -type d -exec chmod 0555 {} + \
    && find /opt/starter -type f -exec chmod 0444 {} +
