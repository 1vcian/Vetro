# ADR 0021 — Editing SQLite databases and SharedPreferences from the file manager

- Status: accepted (M8, on the Linux guest, 2026-09-26). Extends ADR 0020
  (`vetro-files` daemon over virtio-vsock); protocol in
  `docs/specs/files.md` (version 2).

## Context
The M8 exit asks that an edit made from the panel (a value in the
SharedPreferences and **a row of an SQLite database**) be read by the app
after the activity restarts. With ADR 0020 the panel reads databases with
a reader of the format written by us (`web/app/sqlite.mjs`), read-only and
without the WAL, and SharedPreferences can be edited only as XML text.

An app's database is almost always **open** while you look at it: Android
opens databases in WAL (`journal_mode=WAL`, the default since Android 9),
with the `-wal` and `-shm` files next to it and POSIX locks on the bytes of
the file and of the shared memory. Rewriting the file's pages from the host
(or with an atomic write by the daemon, which replaces the inode) would
ignore the app's locks, WAL and page cache: the app would keep reading the
WAL and its memory, or would corrupt the database at its next write.

## Decision

### SQL in the guest with the real SQLite engine
- New **SQL** request (type 14) of the daemon: database path, SQL (one or
  more statements), bound parameters (`NULL`, 64-bit integer, real, text,
  BLOB, by position `?N`), expected number of changed rows and flags
  (read-only). The response carries SQLite's code and its message, changed
  rows, last inserted rowid, columns and rows of the last statement that
  returns any (at most 10000 rows and 16 MiB).
- The daemon executes the SQL with **statically linked SQLite** (official
  *amalgamation* source 3.53.4, sha256 pinned in
  `tools/guest-kernel/build.sh`, public domain), hence with POSIX locks,
  rollback journal or WAL, `-shm` and checkpoints exactly like the app:
  a write from the host is a transaction like the app's.
- Execution happens in a **child process with the uid and gid of the
  owner of the database file** (`setgroups(0)`, `setresgid`,
  `setresuid`): the `-wal`, `-shm` and `-journal` files that SQLite
  creates have the app's owner (a root-owned `-shm` would make the
  database unreadable to the app). After execution the daemon also applies
  the database's SELinux context to those files if it differs (on Android
  the daemon's process does not have the app's MLS categories). The child
  uses a `busy_timeout` of 2 s: if the app holds the write lock longer the
  request fails with `SQLITE_BUSY` and nothing changes.
- All the statements of a request are in **one transaction**
  (`BEGIN IMMEDIATE` … `COMMIT`); with an expected number of changed rows
  (the panel always asks for 1) a difference causes `ROLLBACK` and an
  error: an `UPDATE … WHERE rowid = ?` on a row the app has removed in the
  meantime touches nothing. The rows counted are those changed directly by
  the statements (not by triggers).
- The daemon waits for the child (`poll()` loop stalled for the duration
  of the transaction, at most the `busy_timeout` plus the work):
  acceptable for an interactive manager, the other connections resume
  right after.
- The database must exist (no new databases created by mistake): a path
  that does not exist answers `ENOENT`, a directory `EISDIR`, a special
  file `EINVAL`.
- In the test guest `sqlite3` is a link to `vetro-files` (a multi-call
  program like BusyBox: with `argv[0]` `sqlite3` the official shell
  `shell.c` from the same source starts), so a single copy of the engine
  lives in the initramfs. In the Android image (userdebug) the daemon is
  compiled with bionic from the same source, without the shell: `sqlite3`
  is already in userdebug builds.

Rejected: writing the pages from the host or with the atomic write (see
above); invoking the guest's `sqlite3` shell with the SQL as text (fragile
value quoting, no BLOBs, output to be parsed, and on the Linux guest the
shell would have to be added anyway); an SQLite engine in WebAssembly in
the page (it would work on a copy of the file, same problems as writing
from the host).

### Reading: the viewer also reads the WAL
Reading stays passive (no locks, no process in the guest): the panel reads
the database file and its `-wal` and the reader (`sqlite.mjs`)
reconstructs the last committed snapshot as SQLite does on recovery: WAL
header, cumulative *salt* and checksums of the frames, for each page the
last valid frame up to the last commit frame, which also gives the number
of pages of the database. The `-shm` is not needed. The two reads are not
atomic: if the app checkpoints in between, the inotify event of the file
(or of the `-wal`) makes the database be re-read. Rejected: checkpointing
before reading, which would write into the app's database at every open.

### SharedPreferences: editable table, Android XML
The panel reads the XML (`<map>` with `string`, `int`, `long`, `float`,
`boolean`, `set` of `string`, `null`) with a small XML reader written by
us (no DOM, testable in Node), shows a table with editable type, name and
value, rows to add and remove, and validates values the way Android reads
them back (`Integer.parseInt`, `Long.parseLong`, `Float.parseFloat`,
`true`/`false`). Saving rewrites the file in the format of
`XmlUtils.writeMapXml` with `FastXmlSerializer` (header
`<?xml version='1.0' encoding='utf-8' standalone='yes' ?>`, 4-space
indentation, `<int name="n" value="1" />`, the same escaped characters)
with the daemon's existing atomic write (owner, mode and SELinux context
preserved). A file written by Android and read back without changes gives
the same bytes. The app re-reads the file when it reloads its preferences
(restart of the process or of the activity, as the M8 exit asks): a write
while the app holds the preferences in memory is overwritten at its next
`apply()`, a known limit of every external edit.

### Non-UTF-8 file names
In the protocol names are already bytes. The Rust client keeps paths,
names and link targets as bytes (`Vec<u8>`) and no longer as a `String`
with replaced characters. Towards JavaScript the *surrogateescape*
representation (PEP 383) is used: a byte that is not part of valid UTF-8
becomes the lone surrogate `U+DC80 + (byte - 0x80)`, in JSON `\udcXX`; the
JS sends paths back to vetro-wasm as bytes with the inverse encoding
(`web/node/vetro.mjs`), so any name round-trips. The panel shows those
bytes as `\xNN`. A valid UTF-8 name never contains surrogates (UTF-8 does
not encode them), so the representation is unambiguous.

## Consequences
- Protocol version 2 (SQL request); the client accepts daemons of version
  1 and 2 (with 1 the SQL request answers `ENOSYS`).
- vetro-wasm ABI 9: SQL operation, paths as bytes (no longer rejected if
  not UTF-8), names in surrogateescape.
- The initramfs grows by about 1 MiB (SQLite in the daemon); the daemon
  needs `fork` and the privileges to change user (root in the guest).
- On Android the daemon's init service and its SELinux policy are still
  needed (ADR 0020); the child process's context stays the daemon's, and
  the files it creates take the database's context.
