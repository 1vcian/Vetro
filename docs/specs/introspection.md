# Spec — introspezione del guest dall'esterno (ADR 0027)

Base comune di M7 (hook TLS), M8 (Binder) e M9 (ART/scripting): leggere il
sistema operativo guest dalla sola memoria fisica, senza toccarlo. Codice:
`crates/vetro-analysis/src/introspect/` (senza dipendenze, anche wasm) e
`crates/vetro-machine/src/hooks.rs` + `introspect.rs` (agganci nel ciclo).

## `vetro_analysis::introspect`

### `btf` — tipi del kernel
`Btf::parse(&[u8])` / `Btf::find_in(image) -> Option<(offset, Btf)>`
(cerca il blob BTF nell'`Image`). `offset_of("struct", "campo.campo")`,
`struct_size`, `enum_value`, `member`, `array`, `pointee`, `size_of`.
Salta typedef/const/volatile; risolve i campi dentro unioni e strutture
anonime. Errori `BtfError`, nessun panic su byte arbitrari.

### `kallsyms` — simboli
`Symbols::parse_system_map(&str)` o `Symbols::from_image(image)` (tabella
kallsyms trovata senza simboli; RELA/RELR gestiti). `get(nome)`,
`lookup(addr) -> (simbolo, offset)`, `iter`.

### `mem` — memoria e traduzione
`PhysMem::read_phys(pa, buf)`. `Space { tcr, ttbr0, ttbr1 }`:
`translate(mem, va)`, `read`, `u32/u64`, `cstr`; `with_user(pgd_pa)` per lo
spazio utente. Solo granulo 4 KiB, sola lettura, nessun effetto.
`ttbr_base(ttbr)` toglie ASID e flag.

### `layout` — offset che servono
`Layout::from_btf(&Btf) -> Result<Layout, MissingField>`: task_struct,
mm_struct, vm_area_struct (maple tree), file/path/dentry/inode/mount, cred,
files_struct, fs_struct, signal_struct, vm_struct/page/folio/xarray.

### `linux` — il kernel visto dall'esterno
`Kernel::load(image?, system_map?, btf?)` o `Kernel::new(syms, &btf)`.
`Linux::new(&mem, &kernel, &CpuRegs)` (KASLR da VBAR): `processes`,
`threads`, `all_threads`, `find_pid`, `task`; `vmas`/`maps`,
`cmdline`, `files` (come /proc/<pid>/fd), `fd_file`, `file_path`, `path`;
`cached_page`/`read_file`/`file_bytes` (page cache); `module`,
`module_symbols`, `user_symbol`, `entry_point` (simboli utente dai file in
memoria e da .symtab nella page cache). `Task` porta pid, tgid, comm,
uid/euid/gid, ppid, mm, flags.

### `elf` — simboli utente
`file_symbols(&[u8])` (.symtab/.dynsym dal file), `dynamic_symbols(&mem,
base)` (dalla memoria del processo, conteggio da DT_HASH/DT_GNU_HASH),
`find(syms, nome)`, `header`, `phdr`, `load_bias`.

### `binder` — transazioni grezze (base di M8)
`BINDER_WRITE_READ`, `WriteRead::parse`, `commands(&[u8]) -> Vec<Command>`
(divide il flusso BC/BR), `Command::transaction() -> Option<Transaction>`
(codice, target, flag, dimensioni, indirizzi del Parcel), `interface()`
(descrittore AIDL in testa al Parcel).

### `parcel`, `aidl`, `ipc`, `privacy` — decoder Binder (M8, ADR 0029)
- `parcel::Parcel` legge i tipi di base (`i32`, `i64`, `String16`) e
  `interface_header()` (intestazione `writeInterfaceToken`: strict mode,
  work source, `SYST`/`VNDR`, descrittore); `strings16(&[u8])` tutte le
  stringhe leggibili.
- `aidl::method(descriptor, code) -> Option<&str>` dalla mappa
  dell'immagine (`aidl_aosp15.tsv`, generata da `tools/aosp/aidl-map.sh`)
  più i codici riservati di `IBinder`; `known_interface`, `table_size`.
- `ipc::BinderCall` (mittente/destinatario come `Party` con pid, uid,
  `package()`, interfaccia, metodo, `sensitive`) e `BinderLog` (accoppia
  le due metà BC/BR per codice e byte del Parcel; `to_json`, `line`,
  `sensitive()`).
- `privacy::classify(descriptor, method, strings) -> Vec<Sensitive>`
  (`Category`: posizione, contatti, registro chiamate, sms, calendario,
  appunti, identificativi, fotocamera, microfono, account, app installate).

### `linux::socket_endpoints` — 4-tupla di un fd (M7)
`socket_endpoints(task, fd) -> Option<(SocketAddrV4, SocketAddrV4)>`
(locale, remoto) dalla `struct sock` del kernel (offset dal BTF,
`layout::SockLayout`; `None` se non è un socket IPv4 o il BTF non li ha).
La usa l'hook TLS per legare `SSL*` alla connessione (via il fd di
`connect`).

### `strace` — syscall decodificate
`SyscallRecord` (pid/tid, comm, nr, argomenti, ret, percorso, fd→percorso,
dati letti/scritti, indirizzo del socket, transazioni binder). `line()` in
stile strace; `decode_entry(&user, &fd_path)` e `decode_exit(&user)`.

## `vetro_machine` — agganci nel ciclo (ADR 0027)

- `Machine::set_tracer(Option<Box<dyn Tracer>>)`, `tracer_mut::<T>()`.
- `Machine::trace_syscalls(bool)`: eventi `SyscallEnter`/`SyscallExit` di
  EL0.
- `Machine::add_breakpoint(Breakpoint { va, ttbr0 }) -> id`,
  `remove_breakpoint(id)`, `breakpoints()`: punti d'arresto invisibili su
  indirizzi di EL0 (col JIT gestiti da `SysJit::set_stops`).
- `Machine::with_guest(|GuestView| ...)` e `Machine::linux(&Kernel, |Linux|
  ...)`: leggono il guest fra due quanti.
- `Tracer::event(&Event, &GuestView)`. `Event`: `SyscallEnter(&entry)`,
  `SyscallExit { entry, ret, pc }`, `Breakpoint { id, va, regs }`.
  `GuestView`: `cpu`, `read_phys`, `cpu_regs`, `sp_el0/1`; è `PhysMem`.
- Pronto: `introspect::SyscallTracer` (registra syscall e punti d'arresto
  con la decodifica di `vetro-analysis`), `BreakpointHit`.
- `analysis::{Tracers, BinderTracer, ProcessNames, kernel_profile}` (M8):
  `Tracers` ospita più tracciatori; `BinderTracer` accoppia le transazioni
  in `BinderLog`; `kernel_profile(file, system_map, btf)` carica il profilo
  da un `boot.img` o da un `Image`.
- `tls::{TlsTracer, tls_service, Func}` (M7): punti d'arresto sui simboli
  di `libssl` per processo (risolti da `tls_service` fra due quanti),
  cattura del chiaro di `SSL_write`/`SSL_read`/`_ex` (ritorno via LR),
  4-tupla dal fd di `connect`; `conversations: Vec<TlsConversation>`.

Regole: nulla scrive nel guest; l'esecuzione (istruzioni, interrupt, RAM,
console, snapshot, log) è identica con e senza agganci, anche nel replay.

## Profilo dei kernel di Vetro
- Kernel di prova (Linux 6.18, `target/guest-kernel`): `System.map` +
  `vmlinux.btf` (BTF staccato prodotto da `tools/guest-kernel/build.sh`).
- GKI 6.6 di Android: kallsyms e BTF stanno dentro `boot.img`
  (`BootImage::parse` + `decompress`, poi `Symbols::from_image` e
  `Btf::find_in`).
