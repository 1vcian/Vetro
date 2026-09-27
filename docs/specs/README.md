# Component specs

One spec per component: public interface, invariants, allowed
dependencies, tests that verify its behaviour. Changing an interface
requires an ADR in `docs/adr/` first.

Next to write: `cpu.md` (register state, decoder, memory interface
towards `vetro-mmu`) before starting M1.

Existing specs: `cpu.md`, `mmu.md`, `platform.md`, `net.md`, `jit.md`,
`wasm.md`, `snapshot.md` (M6: snapshot format and API, ADR 0015),
`analysis.md` (M7: network analysis, pcapng, HTTP, HAR, ADR 0016),
`android-boot.md` (M5: boot.img, vendor_boot, init_boot and bootconfig, ADR 0018),
`replay.md` (M10: host inputs, recording, replay and seeking, ADR 0019),
`files.md` (M8: file manager, `vetro-files` daemon over vsock, protocol and
client, ADR 0020; SQL in the guest, WAL, SharedPreferences and non-UTF-8 names,
ADR 0021), `device-profiles.md` (M10: device profile format, boot parameters
and adb settings, ADR 0035).
