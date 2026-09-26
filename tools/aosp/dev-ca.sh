#!/bin/sh
# CA di sviluppo di Vetro nel trust store di sistema dell'immagine AOSP
# (ADR 0030). Serve all'endpoint TLS della sinkhole (hook TLS di M7/M8,
# ADR 0029): i certificati che la sinkhole presenta sono firmati da questa CA,
# e l'immagine di sviluppo se ne fida come di una CA di sistema.
#
#   tools/aosp/dev-ca.sh [check]   controlla certificato, patch e (se c'è) chiave
#   tools/aosp/dev-ca.sh patches   rigenera le patch dal certificato committato
#   tools/aosp/dev-ca.sh new       nuova chiave e nuovo certificato (poi patch);
#                                  rifiuta se la chiave esiste già (--force per
#                                  sostituirla: poi va ricostruita l'immagine)
#
# Nel repository solo il certificato (pubblico):
#   guest/aosp/vendor/vetro/dev-ca/vetro-dev-ca.pem
#   guest/aosp/patches/external/conscrypt/0001-*.patch        (APEX: il trust
#       store che AOSP 15 usa davvero, /apex/com.android.conscrypt/cacerts)
#   guest/aosp/patches/system/ca-certificates/0001-*.patch    (/system/etc/
#       security/cacerts: ripiego con system.certs.enabled=true, tenuto uguale)
# La chiave privata sta fuori dal repository, in VETRO_DEV_CA_DIR
# (predefinita ~/.config/vetro/dev-ca/): vetro-dev-ca.key (PKCS#8 PEM, 0600)
# e una copia di vetro-dev-ca.pem. Se si perde: `new --force` e nuova build.
# Chiave EC P-256 e firme ECDSA-SHA256 (supportate da BoringSSL/Conscrypt da
# sempre); validità 10 anni; basicConstraints CA:TRUE, pathlen:0 (firma solo
# certificati finali), keyUsage keyCertSign e cRLSign.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
pem="$root/guest/aosp/vendor/vetro/dev-ca/vetro-dev-ca.pem"
dir="${VETRO_DEV_CA_DIR:-$HOME/.config/vetro/dev-ca}"
key="$dir/vetro-dev-ca.key"
p_conscrypt="$root/guest/aosp/patches/external/conscrypt/0001-Vetro-CA-di-sviluppo-nel-trust-store.patch"
p_system="$root/guest/aosp/patches/system/ca-certificates/0001-Vetro-CA-di-sviluppo-nel-trust-store.patch"
subject="/O=Vetro/OU=Vetro development builds/CN=Vetro Development CA (not for production)"

die() { echo "dev-ca: $*" >&2; exit 1; }

# Nome del file nel trust store di Android: <subject_hash_old>.0 (README.cacerts
# di system/ca-certificates e di external/conscrypt/apex/ca-certificates).
cahash() { openssl x509 -in "$pem" -noout -subject_hash_old; }

# Contenuto nel formato dei file di AOSP: PEM, testo, impronta SHA-1 (senza
# spazi in coda, che git apply segnalerebbe).
aosp_file() {
  openssl x509 -in "$pem" -outform PEM
  openssl x509 -in "$pem" -noout -text -fingerprint -sha1 | sed 's/[[:space:]]*$//'
}

# patch FILE PERCORSO_NEL_PROGETTO: patch che crea il file (prefissi a/ b/
# relativi al progetto, come le altre di guest/aosp/patches).
write_patch() {
  tmp="$(mktemp)"
  aosp_file > "$tmp"
  n="$(wc -l < "$tmp" | tr -d ' ')"
  mkdir -p "$(dirname "$1")"
  {
    echo "--- /dev/null"
    echo "+++ b/$2"
    echo "@@ -0,0 +1,$n @@"
    sed 's/^/+/' "$tmp"
  } > "$1"
  rm -f "$tmp"
  echo "scritta: ${1#"$root"/}"
}

patches() {
  h="$(cahash)"
  write_patch "$p_conscrypt" "apex/ca-certificates/files/$h.0"
  write_patch "$p_system" "files/$h.0"
}

check() {
  [ -f "$pem" ] || die "manca $pem (tools/aosp/dev-ca.sh new)"
  openssl x509 -in "$pem" -noout -checkend 2592000 >/dev/null || die "il certificato scade entro 30 giorni: tools/aosp/dev-ca.sh new --force"
  openssl x509 -in "$pem" -noout -ext basicConstraints | grep -q 'CA:TRUE' || die "il certificato non è una CA"
  h="$(cahash)"
  cert="$(openssl x509 -in "$pem" -outform PEM)"
  for p in "$p_conscrypt" "$p_system"; do
    [ -f "$p" ] || die "manca ${p#"$root"/} (tools/aosp/dev-ca.sh patches)"
    grep -Eq "^\+\+\+ b/(.*/)?files/$h\.0\$" "$p" || die "${p#"$root"/} non crea $h.0"
    # Il PEM dentro la patch deve essere quello committato.
    got="$(sed -n 's/^+//p' "$p" | sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p')"
    [ "$got" = "$cert" ] || die "${p#"$root"/} ha un certificato diverso da ${pem#"$root"/} (tools/aosp/dev-ca.sh patches)"
  done
  if [ -f "$key" ]; then
    a="$(openssl pkey -in "$key" -pubout)"
    b="$(openssl x509 -in "$pem" -noout -pubkey)"
    [ "$a" = "$b" ] || die "la chiave in $key non corrisponde al certificato committato"
    echo "dev-ca: chiave in $key (corrisponde)"
  else
    echo "dev-ca: chiave privata assente ($key): l'immagine si costruisce lo stesso, la sinkhole TLS no"
  fi
  echo "dev-ca: $h.0 = $(openssl x509 -in "$pem" -noout -subject | sed 's/^subject=//'), scade $(openssl x509 -in "$pem" -noout -enddate | sed 's/^notAfter=//')"
}

new() {
  if [ -f "$key" ] && [ "${1:-}" != --force ]; then
    die "la chiave esiste già ($key): --force per sostituirla (poi nuova build dell'immagine)"
  fi
  mkdir -p "$dir" "$(dirname "$pem")"
  chmod 700 "$dir"
  cfg="$(mktemp)"
  cat > "$cfg" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = ca
prompt = no
[dn]
[ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
EOF
  (umask 077 && openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$key.tmp")
  openssl req -new -x509 -config "$cfg" -key "$key.tmp" -sha256 -days 3650 -subj "$subject" -out "$pem.tmp"
  rm -f "$cfg"
  mv "$key.tmp" "$key"
  mv "$pem.tmp" "$pem"
  cp "$pem" "$dir/vetro-dev-ca.pem"
  echo "chiave: $key (fuori dal repository, non committarla)"
  echo "certificato: ${pem#"$root"/}"
  patches
}

case "${1:-check}" in
  check) check ;;
  patches) patches; check ;;
  new) new "${2:-}"; check ;;
  *) echo "uso: $0 [check|patches|new [--force]]" >&2; exit 2 ;;
esac
