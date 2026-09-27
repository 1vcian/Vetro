#!/bin/sh
# Vetro development CA in the system trust store of the AOSP image
# (ADR 0030). Used by the sinkhole's TLS endpoint (TLS hook of M7/M8,
# ADR 0029): the certificates the sinkhole presents are signed by this CA,
# and the development image trusts it like a system CA.
#
#   tools/aosp/dev-ca.sh [check]   checks certificate, patches and (if present) key
#   tools/aosp/dev-ca.sh patches   regenerates the patches from the committed certificate
#   tools/aosp/dev-ca.sh new       new key and new certificate (then patches);
#                                  refuses if the key already exists (--force to
#                                  replace it: then the image must be rebuilt)
#
# Only the (public) certificate is in the repository:
#   guest/aosp/vendor/vetro/dev-ca/vetro-dev-ca.pem
#   guest/aosp/patches/external/conscrypt/0001-*.patch        (APEX: the trust
#       store AOSP 15 actually uses, /apex/com.android.conscrypt/cacerts)
#   guest/aosp/patches/system/ca-certificates/0001-*.patch    (/system/etc/
#       security/cacerts: fallback with system.certs.enabled=true, kept identical)
# The private key lives outside the repository, in VETRO_DEV_CA_DIR
# (default ~/.config/vetro/dev-ca/): vetro-dev-ca.key (PKCS#8 PEM, 0600)
# and a copy of vetro-dev-ca.pem. If it is lost: `new --force` and a new build.
# EC P-256 key and ECDSA-SHA256 signatures (supported by BoringSSL/Conscrypt
# forever); validity 10 years; basicConstraints CA:TRUE, pathlen:0 (signs only
# end-entity certificates), keyUsage keyCertSign and cRLSign.
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

# File name in the Android trust store: <subject_hash_old>.0 (README.cacerts
# in system/ca-certificates and in external/conscrypt/apex/ca-certificates).
cahash() { openssl x509 -in "$pem" -noout -subject_hash_old; }

# Contents in the AOSP file format: PEM, text, SHA-1 fingerprint (without
# trailing whitespace, which git apply would flag).
aosp_file() {
  openssl x509 -in "$pem" -outform PEM
  openssl x509 -in "$pem" -noout -text -fingerprint -sha1 | sed 's/[[:space:]]*$//'
}

# patch FILE PATH_IN_PROJECT: patch that creates the file (a/ b/ prefixes
# relative to the project, like the others in guest/aosp/patches).
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
  echo "written: ${1#"$root"/}"
}

patches() {
  h="$(cahash)"
  write_patch "$p_conscrypt" "apex/ca-certificates/files/$h.0"
  write_patch "$p_system" "files/$h.0"
}

check() {
  [ -f "$pem" ] || die "$pem missing (tools/aosp/dev-ca.sh new)"
  openssl x509 -in "$pem" -noout -checkend 2592000 >/dev/null || die "the certificate expires within 30 days: tools/aosp/dev-ca.sh new --force"
  openssl x509 -in "$pem" -noout -ext basicConstraints | grep -q 'CA:TRUE' || die "the certificate is not a CA"
  h="$(cahash)"
  cert="$(openssl x509 -in "$pem" -outform PEM)"
  for p in "$p_conscrypt" "$p_system"; do
    [ -f "$p" ] || die "${p#"$root"/} missing (tools/aosp/dev-ca.sh patches)"
    grep -Eq "^\+\+\+ b/(.*/)?files/$h\.0\$" "$p" || die "${p#"$root"/} does not create $h.0"
    # The PEM inside the patch must be the committed one.
    got="$(sed -n 's/^+//p' "$p" | sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p')"
    [ "$got" = "$cert" ] || die "${p#"$root"/} has a certificate different from ${pem#"$root"/} (tools/aosp/dev-ca.sh patches)"
  done
  if [ -f "$key" ]; then
    a="$(openssl pkey -in "$key" -pubout)"
    b="$(openssl x509 -in "$pem" -noout -pubkey)"
    [ "$a" = "$b" ] || die "the key in $key does not match the committed certificate"
    echo "dev-ca: key in $key (matches)"
  else
    echo "dev-ca: private key missing ($key): the image still builds, the TLS sinkhole does not"
  fi
  echo "dev-ca: $h.0 = $(openssl x509 -in "$pem" -noout -subject | sed 's/^subject=//'), expires $(openssl x509 -in "$pem" -noout -enddate | sed 's/^notAfter=//')"
}

new() {
  if [ -f "$key" ] && [ "${1:-}" != --force ]; then
    die "the key already exists ($key): --force to replace it (then a new image build)"
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
  echo "key: $key (outside the repository, do not commit it)"
  echo "certificate: ${pem#"$root"/}"
  patches
}

case "${1:-check}" in
  check) check ;;
  patches) patches; check ;;
  new) new "${2:-}"; check ;;
  *) echo "usage: $0 [check|patches|new [--force]]" >&2; exit 2 ;;
esac
