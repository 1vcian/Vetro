# ADR 0029 — TLS hooks (M7) and Binder decoder (M8) from outside

- Status: accepted (M7–M8, 2026-09-26). Builds on ADR 0027 (introspection
  from outside: invisible syscalls and breakpoints) and ADR 0016
  (network analysis, HAR).
- Number: on rebase onto main, renumber if 0028/0029 are already taken
  (the other agent may take 0028).

## Context
M7 asks for HTTPS requests in plaintext, tied to the action, in the HAR like
the plaintext ones; M8 for decoded Binder transactions (AIDL interface and
method, sender and recipient) and a privacy inspector. Both start from the
hooks of ADR 0027, without touching the guest.

## Decision

### TLS hooks (M7)
- **Plaintext from the hooks, not from the encrypted records.** Invisible
  breakpoints (ADR 0027) on `SSL_write`, `SSL_read`, `SSL_write_ex`,
  `SSL_read_ex` of `libssl.so`, resolved from the ELF symbols of **every**
  process (the system BoringSSL and the one inside Conscrypt,
  `/apex/com.android.conscrypt`). `SSL_write` has the buffer already
  at entry; `SSL_read` does not: the number of bytes is the return value,
  so the buffer is read at return, with a breakpoint on the return address
  (LR) set the first time it is seen.
- **Connection from the syscall sequence, not from BoringSSL's
  structures.** The internal `SSL`/`BIO` structures change between versions;
  the 4-tuple is instead derived from the fd: `connect(fd, ...)` on the same
  thread gives the fd, and `Linux::socket_endpoints` the 4-tuple from the
  kernel's `struct sock` (offsets from BTF: `SockLayout`). Fallback: the first
  connected IPv4 socket among the open fds.
- **Merge with the network analysis.** Each TLS connection becomes a
  `TlsConversation` (to the server = request, from the server = response,
  with guest times); `NetworkAnalysis::merge_tls` reconstructs its HTTP
  requests as `HttpExchange { secure: true, attribution }` and orders
  them with the plaintext ones. In the HAR they appear with `_secure` and
  `_vetro` (pid, process, package, library); in the inspector with `secure`
  and `attribution`.
- **Sinkhole TLS server (to be completed).** For the app's connection to
  complete, an endpoint that speaks TLS is needed: today the sinkhole
  terminates TCP. The plaintext comes from the hooks anyway, so certificate
  validity matters only for the handshake to succeed. Two ways, both with an
  AOSP rebuild or a test app (VM off, see `docs/progress/M7.md`): (a) TLS 1.3
  in the sinkhole with Vetro's development CA (`tls_ports`, default 443) and
  the CA installed in the image (rebuild); (b) a test app that trusts our CA
  or ignores the certificate. The primitives (SHA-256, HMAC, HKDF,
  ChaCha20-Poly1305) are verified with the RFC vectors but not yet
  integrated: they are added with the server, tested against
  `openssl s_client` as the oracle before declaring it done. Way (a) is
  prepared in ADR 0030: EC P-256 development CA (`guest/aosp/vendor/vetro/dev-ca/
  vetro-dev-ca.pem`, key in `~/.config/vetro/dev-ca/`, outside the
  repository) in the conscrypt APEX with a patch; waiting for the build.

### Binder decoder (M8)
- **AIDL map from the image, not by hand.** `tools/aosp/aidl-map.sh`
  extracts the `system` partition (EROFS) from `super.img` and reads with
  `dexdump` the constants AIDL generates in the stubs (`TRANSACTION_*` of
  `<interface>$Stub`, `*_TRANSACTION` of `IContentProvider`):
  descriptor + code → method name, for the running image
  (`aidl_aosp15.tsv`, 1306 interfaces, 11637 methods; including
  IActivityManager, IPackageManager, ILocationManager, ITelephony,
  IContentProvider, IClipboard, ICameraService). The reserved codes of
  `IBinder` (`_PNG`, `_DMP`, ...) are in the code.
- **The two halves of the call.** Each transaction is seen from the sender
  (`BC_TRANSACTION`, with a handle) and from the recipient (`BR_TRANSACTION`,
  with the sender's pid/euid from the kernel); `BinderLog` pairs them by
  code and Parcel bytes, so the call has a sender and a recipient
  (pid, uid, package from the process name).
- **Privacy inspector.** Rules on interface+method and, for content
  providers, on the Parcel strings (authorities, keys such as `android_id`):
  location, contacts, call log, sms, calendar, clipboard,
  identifiers (ANDROID_ID, IMEI/IMSI/ICCID, serial), camera,
  microphone, accounts, installed apps.

### Exposure
`vetro boot --kernel-profile=boot.img [--system-map --kernel-btf] --tls
--binder-log=FILE`: `--tls` puts the HTTPS requests in the HAR (`--har`) and
in the list (`--net-requests`); `--binder-log` writes the calls (JSON or
lines) and prints the sensitive accesses. API in `vetro_analysis::net`
(`TlsConversation`, `merge_tls`), `vetro_analysis::introspect`
(`BinderCall`, `BinderLog`, `privacy`), `vetro_machine`
(`tls::TlsTracer`/`tls_service`, `analysis::{BinderTracer, Tracers}`).
Snapshot at `boot_completed` with `vetro boot --save-on=TESTO:FILE`
(`--save-delay`, `--exit-after-save`), so the tests start from `--restore`.

## Rejected alternatives
- **BoringSSL internal structures for the 4-tuple**: fragile across versions;
  the syscall sequence is stable.
- **Decrypting the captured TLS records**: it would need the session key;
  the hooks give the plaintext directly and without cryptography.
- **Hand-written AIDL table**: it breaks with every image; the compiled
  stubs have the exact codes.
- **TLS server with unverified cryptography**: against the golden rule;
  it is integrated only with an oracle (`openssl s_client`).

## Verification
- Unit tests (`cargo test -p vetro-analysis -p vetro-machine -p vetro-cli`):
  Parcel (SYST header, String16, truncated), AIDL map, `BinderLog`
  (the two halves, ANDROID_ID and clipboard as sensitive), privacy,
  `TlsConversation` → plaintext HTTP with attribution, `merge_tls` in the HAR.
- `vetro boot --tls --binder-log` on the test kernel (without Android): the
  profile loads, the tracers run, no crash (0 calls/0
  conversations, there is no Binder nor libssl).
- **Long test on Android (`VETRO_ANDROID=1`, from snapshot): not yet
  run** (it needs a ~1 h boot and the TLS server or an app that trusts
  the CA). Plan and status in `docs/progress/M7.md` and `M8.md`.

## Consequences
- The exit of M7 (10 apps with HTTPS in plaintext) and of M8 (sensitive
  access on a test app) remain to be closed with Android from snapshot.
- The TLS server with the development CA needs an AOSP rebuild
  (CA in the image's trust store): prepared, not done (VM off).
