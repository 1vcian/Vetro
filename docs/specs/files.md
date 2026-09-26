# File manager: `vetro-files` and its client (M8)

Decisions and rationale in ADR 0020 (base) and ADR 0021 (SQL in the guest,
WAL, SharedPreferences, non-UTF-8 names). Here: the protocol, the interfaces
and the tests. Code: `guest/kernel/initramfs/vetro-files.c` (daemon in the
guest), `crates/vetro-machine/src/files.rs` and `files/proto.rs` (client),
`crates/vetro-wasm/src/files.rs` (ABI 7 and 9), `web/node/vetro.mjs`
(`GuestFiles`), `web/app/files.mjs` and `web/app/sqlite.mjs` (panel),
`crates/vetro-cli/src/files.rs` (`vetro boot --files-*`).

## Transport
virtio-vsock (`docs/specs/platform.md`): the daemon listens on guest port
**5200** (`VMADDR_CID_ANY`), the host connects from CID 2. In the M3
guest `/init` starts it if there is a virtio-vsock device. One single
stream per connection, up to 8 connections at once. In the M3 guest
`/bin/sqlite3` is a link to `vetro-files` (multi-call: with
`argv[0]` `sqlite3` it starts the official SQLite 3.53.4 shell, the same
source linked into the daemon).

## Protocol (version 2)
Everything little endian. `str` = `u16` length + bytes (bytes of the guest
file system, possibly non-UTF-8; no NUL in paths); `bytes` = `u32`
length + bytes. Version 2 adds SQL (type 14); the client accepts
daemons of version 1 and 2 (with 1, SQL answers `ENOSYS`).

Frame: `u32 length` (bytes that follow) · `u8 type` · `u32 id` · body.
The daemon rejects (closes the connection) a request shorter than 5 or
longer than 1 MiB + 16 KiB; the client a frame longer than 64 MiB.

### Greeting (daemon → host, first frame, id 0)
| Type | Body |
|---|---|
| `0x80` HELLO | `u32` magic `"VTRF"` (0x46525456) · `u16` version (2) · `u16` flags (bit 0: `/sys/fs/selinux` exists) · `u32` maximum chunk (1 MiB) |

### Requests (host → daemon)
| Type | Body | Reply (on success) |
|---|---|---|
| 1 STAT | `str` path | `stat` (with `lstat`) |
| 2 LIST | `str` path | `u32` n · n × (`str` name · `stat`), in `strcmp` order, without `.` and `..` |
| 3 READ | `str` path · `u64` offset · `u32` bytes (at most the maximum chunk) | `u64` file size · `bytes` read (fewer than requested = end of file) |
| 4 WOPEN | `u32` handle (chosen by the client) · `str` path · `u32` mode of a new file · `u8` flags (bit 0: fail if the file exists) | empty |
| 5 WDATA | `u32` handle · `u64` offset · `bytes` (at most the maximum chunk) | empty |
| 6 WCOMMIT | `u32` handle | `stat` of the file after the rename |
| 7 WABORT | `u32` handle | empty |
| 8 MKDIR | `str` path · `u32` mode | empty |
| 9 CREATE | `str` path · `u32` mode (empty file, `O_EXCL`) | empty |
| 10 DELETE | `str` path · `u8` flags (bit 0: recursive) | empty |
| 11 RENAME | `str` from · `str` to | empty |
| 12 WATCH | `str` path | `u32` wd |
| 13 UNWATCH | `u32` wd | empty |
| 14 SQL | `str` database · `u8` flags (bit 0: read-only) · `u32` expected changed rows (`0xffffffff` = any) · `bytes` SQL (UTF-8, one or more statements) · `u16` n · n × `value` (parameters `?1`..`?n`) | `u32` SQLite code (0 = success) · `str` message (empty on success); if 0: `u64` changed rows · `i64` last rowid · `u8` truncated · `u16` columns · names (`str`) · `u32` rows · rows × columns × `value` |

`value` = `u8` type · 0 NULL (nothing) · 1 integer (`i64`) · 2 real (`f64`,
IEEE bits) · 3 text (`bytes`, UTF-8) · 4 BLOB (`bytes`).

`stat` = `u8` type (0 other, 1 file, 2 directory, 3 link, 4 char, 5
block, 6 fifo, 7 socket) · `u32` `st_mode` · `u32` uid · `u32` gid · `u64`
size · `i64` mtime (s) · `u32` mtime (ns) · `u32` nlink · `str`
link target (empty if it is not one) · `str` SELinux context
(xattr `security.selinux` without the trailing NUL, empty if absent).

### Replies and events (daemon → host)
| Type | Body |
|---|---|
| `0x81` REPLY (id of the request) | `u32` status (0 or Linux errno) · body from the table above if the status is 0, nothing otherwise |
| `0x82` EVENT (id 0) | `u32` wd (`0xffffffff` for `IN_Q_OVERFLOW`) · `u32` inotify mask · `u32` cookie · `str` name in the watched directory |

Replies arrive in the order of the requests of each connection. The
watched mask is `IN_CREATE | IN_DELETE | IN_MODIFY | IN_CLOSE_WRITE |
IN_MOVED_FROM | IN_MOVED_TO | IN_ATTRIB | IN_DELETE_SELF | IN_MOVE_SELF`
(plus `IN_IGNORED`, `IN_ISDIR` from the kernel). Without inotify in the
kernel WATCH answers `ENOSYS`.

### Semantics
- **Atomic write.** WOPEN resolves a symbolic link to the file it
  points to, rejects directories (`EISDIR`) and special files (`EINVAL`),
  creates `.vetro-tmp.<n>.<name>` in the same directory (`O_EXCL`, 0600).
  WDATA writes with `pwrite`; the first error stays on the handle. WCOMMIT:
  if the file existed, `fchown` to uid and gid, `fchmod` to the mode and copy
  of all xattrs of the old file (fails if `security.selinux` cannot be
  copied); if it is new, uid, gid and `security.selinux` of the directory
  and the mode from WOPEN; then `fsync`, `rename` over the real file, `fsync`
  of the directory. Any error removes the temporary; the handle is always
  released. When the connection closes, open handles are aborted.
- **New files** (WCOMMIT of a file that did not exist, MKDIR, CREATE):
  owner, group and SELinux context of the directory; exact mode
  (`umask` 0).
- **Events** for names starting with `.vetro-tmp.` are not sent.
- **DELETE** on a directory: `rmdir` (empty) or, with the recursive bit,
  depth-first `nftw` without following links.
- **SQL** (ADR 0021): the database must exist (`ENOENT`, `EISDIR`,
  `EINVAL` for a special file). The daemon does `fork`; the child switches to
  uid and gid of the file's owner (`setgroups(0)`, `setresgid`,
  `setresuid`), opens with SQLite (read-only or read-write, never
  create), `busy_timeout` 2 s, runs all statements in `BEGIN
  IMMEDIATE` … `COMMIT` (except in read-only mode) with the parameters bound
  by position to each statement; if the rows changed directly (not
  by triggers) are not the expected ones: `ROLLBACK` and code 19
  (`SQLITE_CONSTRAINT`) with the message "N righe cambiate, attese M:
  annullato" ("N rows changed, M expected: rolled back"). Any SQLite error
  rolls back the transaction. The rows are those of the last statement with
  columns, at most 10000 and 16 MiB (then `truncated`). The parent waits for
  the child (the daemon loop stays blocked) and gives `-wal`, `-shm` and
  `-journal` the owner and SELinux context of the database if different. A
  dead or unresponsive child: `EIO`. Requests are at most 1 MiB + 16 KiB
  (parameters included).

## Client (`vetro_machine::files`)
- `proto`: `PORT`, `MAGIC`, `VERSION`, `CHUNK` (256 KiB), `MAX_FRAME`;
  `MIN_VERSION`; `Request::{encode, decode}`, `Decoder` (frames in arbitrary
  pieces), `Frame::{Hello, Reply, Event}`,
  `parse_stat/list/read/watch/sql`, `Stat`, `Entry`, `Event`, `Kind`,
  `SqlValue::{Null, Int, Real, Text, Blob}`, `SqlResult`, `mask::*`,
  `errno_name`, `display_name` (non-UTF-8 bytes as `\xNN`),
  `encode_sql_args`/`decode_sql_args` (vetro-wasm format); the body
  `encode_*` functions serve the fake daemons of the tests. Paths, names
  (`Entry::name`, `Event::name`) and targets (`Stat::link`) are
  `Vec<u8>`: the guest's bytes.
- `FilesClient::new(port)` / `default()` (5200). Operations (they return
  the id; paths are `impl AsRef<[u8]>`): `stat`, `list`,
  `read(path, offset, bytes | u64::MAX)`, `read_file`,
  `write_file(path, data, mode)`, `create`, `mkdir`,
  `delete(path, recursive)`, `rename`, `watch`, `unwatch`,
  `sql(path, sql, parameters, expected: Option<u32>, read_only)`.
  Outputs: `take_completion() -> Option<Completion { op, result }>` with
  `Outcome::{Stat, List, Data { size, data }, Written(Stat), Watch(wd),
  Sql(SqlResult), Done}` or `FilesError::{Errno, Protocol, Disconnected,
  Sql { code, message }}`;
  `take_event() -> Option<Event>`. State: `state() -> LinkState::{Idle,
  Connecting, Ready(Hello), Waiting { until_ns }}`, `generation()` (greetings
  received), `pending()`. Roots: `roots`, `set_roots`, `app_roots(package)`.
- `pump(&mut Machine)` between one quantum and the next: connection (or new
  attempt after `RETRY_NS` = 100 ms of guest time), `Input::Vsock
  (Recv)` only if `vsock_view` says there are bytes, `Input::Vsock(Send)`
  of the queued requests; it does nothing without vsock, during a replay and
  with the machine stopped on a disk. A dropped connection fails the
  operations already sent (`Disconnected`); those not yet sent wait for the
  next greeting. `close(&mut Machine)`.

## vetro-wasm and JS
ABI 7 (`docs/specs/wasm.md`): bit `VSOCK` (32) of `vetro_machine_new_with`,
`vetro_files_open/close/status/request/pump/take/ptr`; ABI 9: operation
11 `SQL`, paths as bytes, names in JSON in *surrogateescape* (`\udcXX`
for a non-UTF-8 byte). In JS `Machine.files(port)` → `GuestFiles`
(`stat`, `list`, `read`, `writeFile`, `mkdir`, `create`, `delete`,
`rename`, `watch`, `unwatch`, `sql` as Promises; `onEvent`; `status()`;
`pump()`); `pathBytes`/`pathString` (surrogateescape ↔ bytes),
`displayName` (`\xNN`), `encodeSqlArgs` (parameters: null, bigint or integer
→ INTEGER, number → REAL, string → TEXT, Uint8Array → BLOB, boolean →
0/1, or `{ type, value }`), `sqlValue`.

## Web app (`web/app`)
"file manager" option (on by default: the machine has virtio-vsock;
`?nofiles=1` removes it). The Worker holds the `GuestFiles`, advances it
between one slice and the next, records the page's requests in `inputLog` and
sends the page `files-reply`, `files-event`, `files-status`. The panel
(`files.mjs`), next to the screen:
- roots from `?files=/a,/b`, from the "Roots" field or from
  `window.vetroFiles.setRoots([...])` (default `/tmp`, `/root`, `/etc`);
  `window.vetroFiles.state()` for the tests;
- tree: click on a directory = LIST + WATCH (closed: UNWATCH); every event
  rereads the directory (after 100 ms without other events); on every new
  daemon greeting the open directories are reread and re-watched; mode,
  owner, size, target and SELinux context in the title;
- viewers chosen by content (and by extension): SQLite (magic),
  images (PNG, JPEG, GIF, WebP, BMP), JSON, XML (with the SharedPreferences
  table if the root is `<map>`), UTF-8 text, hexadecimal (the
  first 256 KiB); it can be changed by hand;
- editing and "Save" for text, JSON, XML (validated before writing) and
  hexadecimal (bytes can be added and removed): atomic write
  with the file's mode; if the file changes in the guest while it is open it
  reloads by itself, or warns if there are unsaved changes;
- SharedPreferences (XML with root `<map>`): table with type (`string`,
  `int`, `long`, `float`, `boolean`, `set`, `null`), editable name and value,
  entries that can be added and removed; values are checked the way
  Android rereads them and written in its form (`Float.toString` for
  floats); every change rewrites the XML text like
  `XmlUtils.writeMapXml`/`FastXmlSerializer` (`prefsToXml`: an Android file
  reread and rewritten gives the same bytes), then "Save". Our own XML
  reader without DOM (`parseXml`, `parsePrefs`);
- SQLite (`sqlite.mjs`, a reader of the format written by us): tables and
  rows (the first 500), multi-level b-trees, overflow, rowid alias,
  WITHOUT ROWID, **WAL**: the panel also reads `<db>-wal` and the reader
  applies the valid frames (cumulative salts and checksums) up to the last
  commit (`walPages`); the two reads are not atomic, an event on the file
  or on the `-wal` triggers a reread. Editing: click on a cell (type and
  value), "Insert row" (columns with a value or DEFAULT), "✕" on a row, free
  "SQL…"; every time a preview of the query with the parameters
  (editable), then "Run in guest": SQL request to the daemon with
  1 expected row (`WHERE rowid = ?` or, without rowid, the primary key with
  `IS`; builders `updateCellSql`, `deleteRowSql`, `insertRowSql`); after
  the reply the database is reread;
- non-UTF-8 names shown as `\xNN`, reopened with the exact bytes.

## CLI
`vetro boot ... [--vsock] [--files-ls=P]... [--files-cat=P]...
[--files-put=P:FILE]...`: operations in order, results on stdout,
exit 0/1 when they are finished (`crates/vetro-cli/src/files.rs`).

## Tests
- `cargo test -p vetro-machine files`: protocol (exact bytes, frames in
  pieces, corrupted frames, SQL, non-UTF-8 names) and client against a fake
  daemon (reads and writes in pieces, errors in the middle of a write,
  events, dropped connection, roots, SQL with SQLite errors, versions
  1 and 2); `cargo test -p vetro-wasm files` (JSON with surrogateescape and
  SQL values, API without a kernel); `cargo test -p vetro-cli files` (`ls`
  lines).
- `tests/boot/tests/files_modifica.rs` (guest kernel, release, ADR 0021):
  a guest process with uid 10057 keeps a WAL database open;
  UPDATE/INSERT/DELETE from the host reread by the guest with `sqlite3`,
  the app's `-wal` and `-shm` with the database's context, different
  expected rows, missing table and SQL failing halfway that change nothing,
  read-only read with all types, `ENOENT`/`EISDIR`; rollback-journal
  database of another user with no journal left behind; SQL with the owner's
  uid (root's directory: `SQLITE_READONLY`); SharedPreferences
  rewritten with owner and mode preserved; non-UTF-8 name listed,
  read, written and renamed (checked with `od`); two identical
  runs.
- `tests/boot/tests/files.rs` (guest kernel, release): list, reads
  (including 1.2 MB in pieces), errors, writes that preserve mode,
  owner and xattrs (`security.selinux`, `user.*`) read by the guest with
  `stat` and `vetro-dev xattr-get`, through a link, new files with
  the directory's owner and context, large copy compared with
  `cmp`, create/mkdir/rename/delete, no temporary left behind, event from
  a guest process within 1 s of guest time; two identical runs;
  session recorded and replayed identically (ADR 0019).
- `tests/boot/tests/snapshot.rs`: cuts in the middle of a session (read in
  progress, event arriving with the JIT, write in flight with rewind).
- `crates/vetro-cli/tests/boot_files.rs`: `--files-put/ls/cat`, errors.
- `tests/web/files.mjs` (Node, in `tools/web-test.sh`; also SQL on an
  open WAL database seen by the reader in the `-wal`, SharedPreferences
  rewritten by the JS, non-UTF-8 name), `tests/web/unit.mjs` (SQLite on
  `tests/web/testdata/prova.sqlite` and `wal.sqlite` + `-wal`, regenerable
  with `make-sqlite.py`; SQL of the edits; SharedPreferences XML
  reread and rewritten identically, values as in Android; surrogateescape and
  SQL arguments; viewers), `tests/web/browser.mjs` (panel in
  Chrome: live tree, edit and save read back by the guest with
  `cat`, SQLite cell changed from the panel with preview and reread by the
  guest with `sqlite3`, SharedPreferences changed in the table,
  reconnection after snapshot restore).
