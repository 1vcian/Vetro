# External tools to verify the vetro-analysis exports
# (ADR 0016): tshark and tcpdump read back the pcapng, har-validator (schema
# HAR 1.2 by ahmadnassri/har-schema) and haralyzer (Python) reopen the HAR.
# Built by tools/analysis/check.sh on first run.
FROM debian:trixie-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      tshark tcpdump nodejs npm python3 python3-pip ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /opt/har && cd /opt/har \
 && npm install --no-audit --no-fund har-validator@5.1.5 \
 && pip3 install --no-cache-dir --break-system-packages haralyzer==2.4.0
ENV NODE_PATH=/opt/har/node_modules
