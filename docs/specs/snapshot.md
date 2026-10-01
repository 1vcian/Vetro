# `vetro-snapshot` and machine snapshots (M6)

Decisions and reasons in ADR 0015 (and 0028, 0031 for Android and the
prebuilt snapshot). Here: the interface and the format.

## Crate `vetro-snapshot`

No dependencies; compiles for wasm32. Used by `vetro-cpu`, `vetro-mmu`,
`vetro-platform`, `vetro-net` and `vetro-machine` (which re-exports it as
`vetro_machine::vetro_snapshot`).

| Item | Meaning |
|---|---|
| `trait Snapshot { save(&self, &mut Writer); restore(&mut self, &mut Reader) -> Result<()> }` | state that is saved and restored onto an object built with the same configuration |
| `Writer` | `u8/u16/u32/u64/u128` LE, `bool` (0/1), `len_of`, `raw`, `bytes` (u64 + bytes), `str`, `opt` (0 / 1 + value), `seq` (u64 + items), `section(tag, f)` (4 bytes + u64 + content), `put(&impl Snapshot)`; `set_level(Level)` / `level()`: how `compress` compresses in this writer (ADR 0031) |
| `Reader` | the matching reads; `bool` and `opt` reject values other than 0/1; `len_of(min_item)` rejects lengths beyond the bytes left; `section(tag)` gives a reader of the content only, closed with `finish()` (bytes left over = error); `expect_u64(what, expected)` checks a configuration value; `get(&mut impl Snapshot)` |
| `Level` | `Fast` (default: every block with `lz` on its own) or `Small` (frames of blocks with `lzh`: several times slower to write, about a third smaller; for downloaded snapshots). Readers accept both |
| `compress(w, data)` / `decompress_into(r, out, visit)` / `decompress(r)` | large data in blocks of `BLOCK` = 4096 bytes, format of `blocks` (below), at the writer's level. `decompress_into` writes only the blocks present (`out` must be zero, or the caller zeroes the absent ones: `visit` says which are present) |
| `blocks::encode(len, level, block, emit)` / `blocks::decode(source, len, target, visit)` | the same format from and to any storage (the machine's RAM, in chunks) |
| `lz::compress` / `lz::decompress` | LZ77 of one block: LEB128 tokens, even = `t >> 1` literals, odd = copy of `(t >> 1) + 4` bytes from `d` back (LEB128, may overlap) |
| `lzh::Encoder::compress` / `lzh::decompress` | LZ77 + Huffman over a frame of at most `lzh::FRAME` = 256 KiB (below) |
| `hash64(&[u8]) -> u64` | FNV-1a over 8-byte words, rotation and SplitMix64 finaliser; stable (checksum and configuration hash) |
| `encode_file(config_hash, payload)` / `decode_file(bytes) -> (Header, payload)` | header: `"VETROSNP"`, u32 `FORMAT_VERSION`, u64 configuration hash, u64 length, u64 `hash64` of the content. `decode_file` checks magic, version (before anything else), length and checksum |
| `Error` | `BadMagic`, `Version { found, expected }`, `Config { found, expected }`, `Checksum`, `Truncated`, `Section { expected, found }`, `Trailing { section, bytes }`, `Invalid(String)`; `Display` with the reason |

`FORMAT_VERSION` is **4** (2: connections opened by the host, port
forwarding; 3: host frames queued in the `NetLink`, ADR 0019; 4: repeated
blocks and `lzh` frames, ADR 0031).

`encode_container(magic, version, config_hash, payload)` /
`decode_container(magic, version, bytes)`: the same header with another magic
and version (the M10 recording log, `docs/specs/replay.md`);
`encode_file`/`decode_file` are the `"VETROSNP"`/`FORMAT_VERSION` case.

### Blocks (`blocks`, format 4)

u64 length of the data, u64 number of non-zero blocks, then entries until all
of them are covered (zero blocks are absent). An entry: u64 index of its
(first) block, u8 encoding, then:

| Encoding | Written by | Content |
|---|---|---|
| 0 raw | both levels | the block's bytes |
| 1 LZ | `Fast` | u32 length, `lz` of the block |
| 2 same | both levels | u64 index of an earlier block with the same bytes (already written) |
| 3 frame | `Small` | u16 `k` (1–64) blocks, `k - 1` × u64 indices of the others (increasing), u32 length, `lzh` of the concatenated blocks (written raw, as encoding 0 entries, if that is not smaller) |

Every block appears once; entries are in increasing order of index, except
that an encoding-2 entry can follow the frame that holds its source. Repeated
blocks are found by `hash64` and confirmed byte by byte: the same data always
gives the same bytes.

### `lzh` frames

- Code lengths of two canonical Huffman codes, 4 bits each (low nibble
  first): 296 literal/length symbols (bytes 0–255, then 40 length codes) and
  44 distance codes; 0 = unused, at most 12 bits (170 bytes).
- A bit stream, least significant bit first, until the frame (whose length
  the caller knows) is full: a symbol below 256 is a byte; otherwise a length
  code (value `len - 4`), its extra bits, a distance code (value `distance -
  1`, up to 256 KiB) and its extra bits.
- Values: `v < 16` is code `v`; otherwise, with `n` the index of the highest
  bit of `v` and `m` the bit below it, code `16 + 2 (n - 4) + m` and the
  `n - 1` low bits of `v` as extra bits.
- Compressor: hash chains (depth 48) over the whole frame, one step of lazy
  matching; lengths limited with the JPEG Annex K.3 method. Decoder: one
  4096-entry table per code; any damage is an error or caught by the file
  checksum, never a panic.

## `vetro-machine`

- `Machine::save(&self) -> Vec<u8>` (fast level) and
  `Machine::save_with(&self, level)`: they do not change the machine; between
  any two `run`s.
- `Machine::save_stream(&self, reserve, sink) -> [u8; HEADER_LEN]` and
  `save_stream_with(level, reserve, sink)` (ADR 0028): the same file in
  chunks, for wasm32 with Android: the content goes to `sink` in order, the
  header is the result; the RAM (`Ram::save_chunks(level, emit)`, about 1 MiB
  chunks) is compressed twice, the first time for the length that enters the
  hash. `reserve`: expected size of the sections before the RAM.
- `Machine::load_state_stream(&mut self, head, pull)` (ADR 0028): chunked
  restore: `head` = the file up to and including the header of the `RAM `
  section, `pull` provides the rest (`Ram::restore_from` with a `RamSource`).
  Magic, version and configuration are checked before the machine is
  touched; the checksum (chunked hash64) at the end.
- `Machine::load_state(&mut self, &[u8]) -> Result<(), Error>`: onto the
  machine built and completed like the saved one (same `MachineConfig` and
  `Devices`, same devices mounted afterwards with their external backends).
  The JIT, if any, stays.
- `Machine::restore(&MachineConfig, &Devices, &[u8]) -> Result<Machine, Error>`:
  `with_devices` + `load_state`.
- `Machine::config_hash()`. With one core the configuration bytes (hence
  the hash, and the prebuilt snapshots' keys) are those of the single-core
  machine; with more they add `"cpus"` and the number (ADR 0042).

Content sections, in order:

| Tag | Fields |
|---|---|
| `MACH` | u64 instructions; opt u64 timer deadline; opt u64 network deadline; bool WFI pending; u64 CNTPCT; bool lines to update; bool virtio to serve; bool disk waiting |
| `CPU ` | `Cpu` (of the running core with several cores): 31 × u64 X, u64 SP, u64 PC, u32 NZCV, u64 TPIDR_EL0, u64 TPIDRRO_EL0, opt monitor (u64 address, u32 bytes, u128 value), 32 × u128 V, u32 FPCR, u32 FPSR, then `SysState`: u8 mode, `SysConfig` (u8 PSCI, u64 MPIDR, bool GICv3, u64 CBAR), u8 EL (0/1), bool SPSel, u32 DAIF, bool IL, 2 × u64 SP_ELx, 17 × u64 EL1 registers (ELR, SPSR, VBAR, ESR, FAR, SCTLR, TCR, TTBR0, TTBR1, MAIR, CONTEXTIDR, CPACR, TPIDR, PAR, CNTKCTL, CSSELR, MDSCR), bool OSLK, u64 OSDLR, 20 × u64 DBGB/WVR/CR, u8 CLAIM, u64 PMUSERENR, opt u32 SError |
| `SMP ` | only with several cores (ADR 0042): u64 cores, u64 running core, u64 clock value at which its turn ends, then per core bool on and, for the others, bool WFI pending and its `Cpu` |
| `MMU ` | u64 PARange (checked), 5 × u64 translation registers, u32 TLB entries, per entry u32 slot (increasing), u64 VA base, u64 size, u64 PA base, u16 ASID, bool global, u8 level, u8 AP, bool UXN, bool PXN, u8 AttrIndx, u8 SH |
| `PLAT` | sections `TIMR` (core 0's timer), `GIC3`, `UART`, `RTC `, `GPIO`, then 32 × `VIO ` (u64 slot, transport, queues, section `VDEV` with the device if any); with several cores then `TMRS` (the timers of cores 1..n). `GIC3` with one core is the single-core layout; with more it is followed, per core 1..n, by its 32 private interrupts, its wake state and its CPU interface |
| `RAM ` | `blocks` of the whole RAM |

## `vetro-platform`

- `VirtioDevice::save_state(&self, &mut Writer)` and
  `restore_state(&mut self, &mut Reader) -> Result<()>` (required).
- `BlockBackend`, `NetBackend`, `ConsoleBackend`: `save_state` /
  `restore_state` with an empty default (external link). They save:
  `MemBackend` (writable: the content; read-only: hash), `CowBackend` (u64
  size, written clusters each with `compress`, then the base), `QueueNet`,
  `BufferConsole`.
- `Snapshot` for `Virt`, `VirtioMmio`, `Virtqueue`, `Gic`, `GenericTimer`,
  `Pl011`, `Pl031`, `Pl061`; `DescChain::save/restore`.
- `VirtioGpu::restore_state` hands image (or `disable`) and cursor of every
  scanout back to the `DisplayBackend`.

## `vetro-net`

`impl<U: Upstream + Snapshot> Snapshot for Stack<U>` and
`impl Snapshot for Sinkhole` (in `stack/snapshot.rs`, `sinkhole/snapshot.rs`,
`tcp/snapshot.rs`). The stack and sinkhole configuration is not saved.

## `vetro-cpu`, `vetro-mmu`

`impl Snapshot for Cpu`, `SysState`, `SysConfig` (`vetro-cpu/src/snapshot.rs`);
`impl Snapshot for Mmu`, `Tlb`. On restore the MMU empties the cache of recent
translations and increments `Tlb::flushes`.

## Host

- `vetro boot --save-at=INSTRUCTIONS:FILE` (repeatable: first quantum
  boundary with at least that many instructions) and `--restore=FILE`
  (without `--kernel`; same machine options and same `--disk`).
- `vetro boot --disk=FILE --overlay=FILE`: persistent overlay (below).
- vetro-wasm (`docs/specs/wasm.md`): ABI 4 `vetro_snapshot_version`,
  `vetro_snapshot_save`, `vetro_snapshot_ptr`, `vetro_snapshot_clear`,
  `vetro_snapshot_restore` with codes `OK`, `BAD_MAGIC`, `VERSION`, `CONFIG`,
  `CORRUPT`; ABI 12 chunked save and restore; ABI 13
  `vetro_snapshot_config_hash`, `vetro_snapshot_set_level`.
- The prebuilt Android snapshot (ADR 0031): `docs/specs/wasm.md`.

## Tests

- `vetro-snapshot`: integers, sections, impossible lengths, header (version,
  checksum, truncation, magic), fixed hash, LZ (damaged blocks too), zero
  blocks; `blocks` (both levels round trip and are deterministic, repeated
  blocks, frames written raw when they do not shrink, damaged data rejected,
  copies of blocks not written yet and blocks written twice rejected);
  `lzh` (value buckets, limited and complete code lengths, long runs, far
  matches, skewed bytes, damaged frames never panic).
- `vetro-cpu` `cpu_completa_andata_e_ritorno`, `vetro-mmu`
  `tlb_nello_snapshot`, `vetro-net` `stack_ripristinato_prosegue_uguale`.
- `vetro-machine` `machine::snapshot::tests` (bare-metal probe, edge cases,
  restore onto a used machine, incompatible snapshots, virtio-blk in flight,
  `small_level_restores_the_same_state`: the small level whole and chunked,
  restored whole and in chunks, gives the same machine and the same
  continuation).
- `tests/boot/tests/snapshot.rs` (guest kernel, release,
  `VETRO_REQUIRE_GUEST_KERNEL=1`): equivalence with cuts during boot, shell,
  network, disk, GPU/input/vsock, interpreter and JIT; measurements.
- `crates/vetro-cli/tests/boot_snapshot.rs`, `vetro-wasm`
  `snapshot_dall_api` (also ABI 13).

## Persistent disk overlay (`vetro_snapshot::overlay`, ADR 0017)

File of the guest's writes to a copy-on-write disk, the same for the CLI and
for the browser (OPFS). Little endian.

| Part | Content |
|---|---|
| header (4096 bytes) | `"VETROCOW"`, u32 version (1), u32 cluster (4096), u64 disk size, u64 generation, u64 slots, u32 identity length, identity (at most 4032 bytes), zeros, u64 `hash64` of the first 4088 bytes at offset 4088 |
| slot k (offset 4096 + k × 4112) | u64 cluster (`u64::MAX` = free), u64 check = `hash64(data) ^ rotl(cluster, 17) ^ constant`, 4096 bytes of data (the disk's last cluster padded with zeros) |

- `Overlay::load(file, identity, size)`: empty file = new overlay; different
  identity or size = `LoadError::Mismatch`; magic, version, cluster or damaged
  header = `LoadError::Corrupt` (in both cases it starts from an empty overlay
  and the file is truncated). Slots with a wrong check, an index outside the
  disk or repeated: ignored (counted in `damaged`) and free; slots past the
  end of the file: free.
- `Overlay::update(changes)`: `(cluster, Some(data))` writes (if the check
  changed) into the cluster's slot, or a free one, or at the end;
  `(cluster, None)` frees the slot. `Overlay::sync(all)`: the complete state
  (removes absent clusters). They return `Patches { truncate, writes }`; if
  there is anything the generation grows and the last write is the header.
  `Patches::encode` is the encoding of `vetro_overlay_take`.
- `CowBackend` (vetro-platform): `take_dirty` (clusters written by the guest
  since last time; not in snapshots, emptied by `restore_state`), `cluster`,
  `clusters`, `load_cluster`.
- CLI (`vetro-cli/src/disk.rs`): `FileOverlay::open/persist/after_restore`,
  identity `file:<name>|<size>|<mtime in ns>`; writes are applied with
  `write_at`, the header after `sync_data`. `vetro boot` saves at every
  quantum boundary and at exit, and after `--restore` realigns the file with
  the snapshot's clusters.
