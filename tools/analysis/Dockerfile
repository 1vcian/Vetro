# Strumenti esterni per verificare le esportazioni di vetro-analysis
# (ADR 0016): tshark e tcpdump rileggono il pcapng, har-validator (schema
# HAR 1.2 di ahmadnassri/har-schema) e haralyzer (Python) riaprono l'HAR.
# Costruita da tools/analysis/check.sh alla prima esecuzione.
FROM debian:trixie-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      tshark tcpdump nodejs npm python3 python3-pip ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /opt/har && cd /opt/har \
 && npm install --no-audit --no-fund har-validator@5.1.5 \
 && pip3 install --no-cache-dir --break-system-packages haralyzer==2.4.0
ENV NODE_PATH=/opt/har/node_modules
