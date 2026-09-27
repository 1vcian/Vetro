# ADR 0020 — File manager: Vetro daemon in the guest over virtio-vsock

- Status: accepted (M8, base on the Linux guest, 2026-09-26). Uses
  virtio-vsock (M5, `docs/specs/platform.md`), the host's single entry
  point (ADR 0019) and snapshots (ADR 0015).

## Context
M8 asks for a file manager for the foreground app (`docs/PLAN.md`):
tree of `/data/data/<package>`, `/data/user_de/0/<package>`,
`/sdcard/Android/{data,media}/<package>`, updated live, viewers (text,
JSON, SharedPreferences XML, SQLite, images, hex) and editing with
immediate save into the guest; owner, permissions and SELinux context
preserved; every user edit is a recorded input, so the replay stays
identical.

Reading and writing the disk image from the host (ext4/f2fs in the overlay
file) is ruled out: with the guest running the kernel has caches, journal
and metadata in memory, and a write from outside corrupts the file system;
even a read from outside sees a stale state. Files must be read and
written **by the guest kernel**, from a process with the necessary
privileges (root in the userdebug image).

Android is not there yet; the M3 Linux guest (BusyBox, initramfs) is
enough to build and test the mechanism.

## Decision

### A Vetro daemon in the guest over virtio-vsock
- `vetro-files` (`guest/kernel/initramfs/vetro-files.c`): static C (musl
  today, bionic in the Android image), a single process with a `poll()`
  loop, up to 8 connections, each with its own inotify and a non-blocking
  output buffer (a host that does not read does not stop the daemon;
  beyond 8 MiB of pending output the daemon stops reading requests and
  events of that connection). Only POSIX and Linux UAPI headers.
- **Fixed vsock port 5200**, listening from any CID; the host (CID 2)
  connects (like adb over vsock). If nobody is listening yet (daemon not
  started) the client retries every 100 ms of guest time.
- In the M3 guest `/init` starts it when there is a virtio-vsock device
  (id 19 in `/sys/bus/virtio/devices/*/device`): under QEMU (without
  vsock) it does not start and the comparison log does not change. In the
  Android image it will be an init service (`vetro_files`, its own SELinux
  domain or `su` in userdebug); not in this work.
- The guest kernel now has `CONFIG_INOTIFY_USER` (live events) and
  `CONFIG_TMPFS_XATTR` (xattrs on tmpfs: the tests prove that xattrs,
  including `security.selinux`, are preserved; without an LSM tmpfs keeps
  it as any other xattr). The CI cache renews itself (key on
  `guest/kernel/config/**` and `guest/kernel/initramfs/**`).

### Small, versioned binary protocol
Details in `docs/specs/files.md`. Frames `u32 length, u8 type, u32 id,
body` in little endian; the daemon opens every connection with a greeting
(`"VTRF"`, version 1, SELinux flag, maximum chunk 1 MiB). Requests: STAT,
LIST (with the metadata of each entry: type, mode, uid, gid, size, mtime,
nlink, link target, context from `security.selinux`), READ (in chunks:
offset and length), three-step write (WOPEN/WDATA/WCOMMIT, WABORT),
MKDIR, CREATE, DELETE (recursive too), RENAME, WATCH/UNWATCH (inotify);
responses with Linux errno; unsolicited inotify events. New version = new
number in the greeting: the client rejects a version it does not know.

### Atomic write that preserves metadata
- WOPEN creates a temporary file `.vetro-tmp.<n>.<name>` **in the same
  directory** (same file system: the rename is atomic) with `O_EXCL`; the
  WDATAs write into it; WCOMMIT copies onto the temporary file the owner
  and group (`fchown`, before `fchmod`: chown clears setuid/setgid), mode
  and **all the xattrs** of the file it replaces (`security.selinux`
  mandatory: if it cannot be copied the write fails), `fsync`, `rename`
  onto the real file, `fsync` of the directory. Whoever reads the file
  sees the old or the new one, never something in between; an error at
  any step removes the temporary file and the real file does not change.
- A symbolic link stays: the file it points to is replaced.
- A **new** file (or directory) takes the owner, group and SELinux
  context of the directory that contains it, and the mode requested by the
  host (`umask` 0): this is what Android does for an app's files, all with
  the uid and context (MLS categories included) of its data directory.
- inotify events of temporary files do not reach the host: the write
  shows up as `IN_MOVED_TO` of the real file.
- Known limit: the rename breaks hard links (the real file becomes a new
  inode), like every editor that saves atomically.

### Host client in Rust, without dependencies
- `vetro_machine::files` (`proto`: frame encoding and decoding; client
  `FilesClient`), wasm32 without dependencies. The client touches the
  machine only with `Machine::input(Input::Vsock(..))` (connect, send,
  read) and `Machine::vsock_view` (state and ready bytes): it reads only
  when there are bytes, so the log of an idle session stays empty.
- Asynchronous operations with an id and a `Completion`: a long read
  becomes 256 KiB READs one after the other; a write becomes
  WOPEN + WDATA + WCOMMIT sent together (the daemon serves them in order,
  a failed WDATA makes the WCOMMIT fail).
- The **roots to show** are an interface the caller sets
  (`FilesClient::set_roots`, `window.vetroFiles.setRoots` in the page,
  `app_roots(package)` for an app's directories): today by hand or from
  the URL, in the future from the detection of the foreground app in the
  Binder decoder (ActivityTaskManager), which is not part of this work.
- Exposed by `vetro-wasm` (ABI 7, `vetro_files_*`, responses in JSON plus
  the bytes read), by `web/node/vetro.mjs` (`GuestFiles`, Promise) and by
  `vetro boot --files-ls/--files-cat/--files-put` for the tests.

### Determinism
The host's requests enter the guest as host→guest vsock bytes, and
**all** of them go through the single entry point `Machine::input` (ADR
0019): the host's connect, send and read are `Input::Vsock(..)` recorded
with the instruction number. A recorded file manager session replays
identically without a client (test `files.rs`); the same script gives the
same instructions and the same responses. The client must be called
between one quantum and the next (like the console), not with the machine
stopped on a disk (the inputs would be deferred: the client does nothing
in that case) nor after shutdown (an input after the last instruction
would never reach the guest and the replay would report it as missed).

### Snapshot
Connection, credits and bytes in flight are virtio-vsock state (in the
snapshot); the client is a host link. A cut in the middle of a session
with the same client continues identically (test in `snapshot.rs`). A new
session (page reopened from a cached snapshot) creates a new client: on
first connect it closes with RST the connections to the daemon's port left
in the snapshot, which nobody would read any more.

## Consequences
- The file manager works on today's Linux guest and in the web app (panel
  next to the screen); for Android we need the init service and the
  daemon's SELinux policy, the detection of the foreground app and the
  package roots.
- The daemon has the guest's root privileges: it reads and writes any
  file (that is the point); this holds only for Vetro's userdebug images.
- SQLite is read in the page with a reader of the format written by us
  (`web/app/sqlite.mjs`, read-only, without WAL); editing a row (M8 exit)
  remains to be done: writing it in the file format is risky with the app
  holding the database open, and it will have to be done with `sqlite3` in
  the guest or in the daemon, with an ADR. *Done with ADR 0021: SQL in the
  daemon with SQLite linked in, like the database's owner; WAL reading.*
- Non-UTF-8 file names reach the host with the characters replaced
  (U+FFFD) and cannot be reopened: known limit, rare on Android.
  *Superseded by ADR 0021: names as bytes, surrogateescape towards the
  JS.*
