#!/bin/sh
# Rilegge con strumenti esterni le esportazioni di vetro-analysis (ADR 0016),
# in un container Debian (tools/analysis/Dockerfile, costruito al primo uso):
#   check.sh FILE.pcapng FILE.har
# Righe prodotte (una per fatto, facili da confrontare nei test):
#   CAPINFOS-PACKETS n          capinfos: pacchetti nel file
#   TCPDUMP-PACKETS n           tcpdump -r: pacchetti letti
#   TSHARK-REQUEST metodo uri   tshark: richieste HTTP decodificate
#   TSHARK-RESPONSE codice      tshark: risposte HTTP
#   HAR-VALID                   har-validator (schema HAR 1.2) accetta il file
#   HARALYZER n                 haralyzer (Python) apre il file: n voci
#   HARALYZER-ENTRY metodo url stato
# VETRO_HTTP_PORTS (porte separate da virgole) dice a tshark di decodificare
# come HTTP anche porte diverse dalle solite.
set -eu
IMAGE="${VETRO_ANALYSIS_IMAGE:-vetro-analysis-tools:latest}"
here="$(cd "$(dirname "$0")" && pwd)"
if [ $# -ne 2 ]; then
  echo "uso: $0 FILE.pcapng FILE.har" >&2
  exit 2
fi
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" "$here" >&2
fi
pcap_dir="$(cd "$(dirname "$1")" && pwd -P)"
har_dir="$(cd "$(dirname "$2")" && pwd -P)"
pcap="$pcap_dir/$(basename "$1")"
har="$har_dir/$(basename "$2")"
decode=""
IFS=','
for p in ${VETRO_HTTP_PORTS:-}; do
  [ -n "$p" ] && decode="$decode -d tcp.port==$p,http"
done
unset IFS
# shellcheck disable=SC2086,SC2016
exec docker run --rm -i -v "$pcap_dir:$pcap_dir:ro" -v "$har_dir:$har_dir:ro" \
  -e PCAP="$pcap" -e HAR="$har" -e DECODE="$decode" "$IMAGE" sh -eu -c '
echo "CAPINFOS-PACKETS $(capinfos -c -M "$PCAP" | awk -F: "/Number of packets/ {gsub(/ /, \"\", \$2); print \$2}")"
echo "TCPDUMP-PACKETS $(tcpdump -nn -r "$PCAP" 2>/dev/null | wc -l)"
tshark -n -r "$PCAP" $DECODE -Y http.request -T fields -e http.request.method -e http.request.uri \
  | sed "s/^/TSHARK-REQUEST /; s/\t/ /"
tshark -n -r "$PCAP" $DECODE -Y http.response -T fields -e http.response.code | sed "s/^/TSHARK-RESPONSE /"
node -e "
const fs = require(\"fs\");
require(\"har-validator\").har(JSON.parse(fs.readFileSync(process.env.HAR, \"utf8\")))
  .then(() => console.log(\"HAR-VALID\"))
  .catch(e => { console.log(\"HAR-INVALID \" + JSON.stringify(e.errors || e.message)); process.exit(1); });
"
python3 - <<EOF
import json, os
from haralyzer import HarParser
with open(os.environ["HAR"]) as f:
    p = HarParser(json.load(f))
entries = [e for page in [p] for e in page.har_data["entries"]]
print("HARALYZER", len(entries))
for e in entries:
    print("HARALYZER-ENTRY", e["request"]["method"], e["request"]["url"], e["response"]["status"])
EOF
'
