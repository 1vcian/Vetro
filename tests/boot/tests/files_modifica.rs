//! The M8 file manager editing (ADR 0021) on the M3 guest kernel: SQL in
//! the guest with the SQLite engine of the `vetro-files` daemon,
//! rewritten SharedPreferences, non-UTF-8 file names.
//!
//! - a guest process (uid 10057, like an app) creates a database in WAL
//!   and keeps it open; the host changes a row with SQL and bound parameters,
//!   inserts one and removes one: the guest rereads the new values with
//!   `sqlite3`, the `-wal` stays the app's (owner and SELinux context
//!   of the database), a request that would change a different number of
//!   rows or with wrong SQL touches nothing, a read-only read
//!   returns rows with all types;
//! - a rollback-journal database of another user, with nobody
//!   keeping it open: change read back, no journal left;
//! - SharedPreferences: the XML file rewritten like Android by the host, reread
//!   by the guest with owner and mode preserved;
//! - a name with non-UTF-8 bytes listed, read, written and renamed with
//!   its exact bytes (the guest checks them with `od`);
//! - determinism: two runs give the same log, the same instructions
//!   and the same responses.
//!
//! Release only, like `files.rs`.

use vetro_boot_tests::*;
use vetro_machine::files::proto::{Kind, SqlResult, SqlValue};
use vetro_machine::files::{FilesError, Outcome};
use vetro_machine::{Devices, FilesClient, Input, Machine, MachineConfig, Stop};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;
const DB: &str = "/tmp/app/databases/app.db";

fn booted(image: &[u8], initrd: &[u8]) -> Machine {
    let devices = Devices { vsock_cid: Some(3), ..Devices::default() };
    let mut m = Machine::with_devices(&MachineConfig::default(), &devices);
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("kernel load");
    m
}

struct Script {
    m: Machine,
    fc: FilesClient,
    log: Vec<u8>,
    seen: Vec<String>,
}

impl Script {
    fn quantum(&mut self) -> Stop {
        let s = self.m.run(QUANTUM);
        self.log.extend(self.m.console_output());
        if s == Stop::Budget {
            self.fc.pump(&mut self.m);
        }
        while let Some(e) = self.fc.take_event() {
            self.seen.push(format!("event {e:?}"));
        }
        s
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} did not arrive:\n{}", self.tail());
            let stop = self.quantum();
            assert_eq!(stop, Stop::Budget, "{stop:?} while waiting for {needle:?}:\n{}", self.tail());
        }
    }

    /// A command at the shell: its output (between two markers).
    fn command(&mut self, cmd: &str) -> String {
        let from = self.log.len();
        let line = format!("echo VETRO-OUT-\"\"INIZIO; {cmd}; echo VETRO-OUT-\"\"FINE\n");
        self.m.input(Input::Console(line.into_bytes()));
        let end = self.until("VETRO-OUT-FINE", from);
        self.until(SHELL_PROMPT, end);
        let out = normalize(&String::from_utf8_lossy(&self.log[from..end]));
        let start = out.find("VETRO-OUT-INIZIO\n").expect("marker") + "VETRO-OUT-INIZIO\n".len();
        out[start..out.len() - "VETRO-OUT-FINE".len()].trim_end_matches('\n').to_string()
    }

    fn wait(&mut self, op: u32) -> Result<Outcome, FilesError> {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            while let Some(c) = self.fc.take_completion() {
                let brief = match &c.result {
                    Ok(Outcome::Data { size, data }) => format!("Data {size} {} byte", data.len()),
                    r => format!("{r:?}"),
                };
                self.seen.push(format!("op {} {brief}", c.op));
                if c.op == op {
                    return c.result;
                }
            }
            assert!(self.m.steps < limit, "operation {op} not finished:\n{}", self.tail());
            assert_eq!(self.quantum(), Stop::Budget, "{}", self.tail());
        }
    }

    fn ok(&mut self, op: u32) -> Outcome {
        self.wait(op).unwrap_or_else(|e| panic!("operation {op}: {e}\n{}", self.tail()))
    }

    fn sql(
        &mut self,
        sql: &str,
        params: Vec<SqlValue>,
        expect: Option<u32>,
    ) -> Result<SqlResult, FilesError> {
        let op = self.fc.sql(DB, sql, params, expect, false);
        self.wait(op).map(|o| match o {
            Outcome::Sql(r) => r,
            o => panic!("{o:?}"),
        })
    }

    /// The guest's `sqlite3` as the app (uid 10057).
    fn app_sqlite(&mut self, sql: &str) -> String {
        self.command(&format!("vetro-dev run-as 10057 10057 sqlite3 -batch -list {DB} \"{sql}\""))
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// SharedPreferences as Android writes them (XmlUtils.writeMapXml).
const PREFS: &str = "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map>\n    \
                     <string name=\"nome\">prima</string>\n    <int name=\"avvii\" value=\"3\" />\n    \
                     <boolean name=\"primo\" value=\"true\" />\n</map>\n";

fn session(image: &[u8], initrd: &[u8]) -> (Vec<u8>, u64, Vec<String>) {
    let m = booted(image, initrd);
    let mut s = Script { m, fc: FilesClient::default(), log: Vec::new(), seen: Vec::new() };
    s.until(SHELL_PROMPT, 0);

    // ---- An "app" with its database in WAL, kept open ------------------------
    let out = s.command(
        "mkdir -p /tmp/app/databases /tmp/app/shared_prefs && chown -R 10057:10057 /tmp/app && chmod 700 /tmp/app \
         && vetro-dev xattr-set /tmp/app/databases security.selinux u:object_r:app_data_file:s0:c57",
    );
    assert_eq!(out, "");
    let out = s.app_sqlite(
        "PRAGMA journal_mode=WAL; CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, n REAL); \
         INSERT INTO t VALUES (1, 'uno', 1.5), (2, 'due', NULL);",
    );
    assert_eq!(out, "wal");
    s.command(&format!("vetro-dev xattr-set {DB} security.selinux u:object_r:app_data_file:s0:c57"));
    // The app's process: a read (opens -wal and -shm), then it stays.
    let from = s.log.len();
    s.m.input(Input::Console(
        format!(
            "(echo 'SELECT count(*) FROM t;'; sleep 100000) | vetro-dev run-as 10057 10057 sqlite3 {DB} >/dev/null &\n"
        )
        .into_bytes(),
    ));
    s.until(SHELL_PROMPT, from);
    let limit = s.m.steps + PHASE_BUDGET;
    while !s.command("ls /tmp/app/databases").contains("app.db-shm") {
        assert!(s.m.steps < limit, "the app did not open the database:\n{}", s.tail());
    }

    // ---- Changes from the host -------------------------------------------------
    let r = s
        .sql(
            "UPDATE t SET v = ?1 WHERE rowid = ?2",
            vec![SqlValue::Text("modificato dall'host".into()), SqlValue::Int(1)],
            Some(1),
        )
        .unwrap_or_else(|e| panic!("UPDATE: {e}\n{}", s.tail()));
    assert_eq!((r.changes, r.columns.len()), (1, 0));
    let r = s
        .sql(
            "INSERT INTO t (v, n) VALUES (?1, ?2)",
            vec![SqlValue::Text("tre".into()), SqlValue::Real(-0.25)],
            Some(1),
        )
        .unwrap();
    assert_eq!((r.changes, r.last_rowid), (1, 3));
    let r = s.sql("DELETE FROM t WHERE rowid = ?1", vec![SqlValue::Int(2)], Some(1)).unwrap();
    assert_eq!(r.changes, 1);
    // The guest (as the app) rereads the new values with sqlite3.
    let out = s.app_sqlite("SELECT id, v, n FROM t ORDER BY id");
    assert_eq!(out, "1|modificato dall'host|1.5\n3|tre|-0.25");
    // The app keeps the database: the changes are in the -wal, which stays
    // the app's (owner and context of the database).
    let out = s.command(
        "cd /tmp/app/databases && stat -c '%n %u %g %a' app.db app.db-wal app.db-shm; \
         test -s app.db-wal && echo WAL-NON-VUOTO; vetro-dev xattr-get app.db-wal security.selinux; cd /",
    );
    assert_eq!(
        out,
        "app.db 10057 10057 644\napp.db-wal 10057 10057 644\napp.db-shm 10057 10057 644\nWAL-NON-VUOTO\n\
         vetro-dev: xattr security.selinux=u:object_r:app_data_file:s0:c57"
    );
    // Changed rows different from those expected: rolled back, nothing changes.
    let e = s.sql("UPDATE t SET v = 'no' WHERE rowid = ?1 OR rowid = 3", vec![SqlValue::Int(1)], Some(1));
    assert!(
        matches!(&e, Err(FilesError::Sql { code: 19, message }) if message.contains("2 righe cambiate")),
        "{e:?}"
    );
    let e = s.sql("UPDATE t SET v = 'no' WHERE rowid = ?1", vec![SqlValue::Int(99)], Some(1));
    assert!(matches!(&e, Err(FilesError::Sql { code: 19, .. })), "{e:?}");
    let e = s.sql("UPDATE tt SET v = 1", vec![], None);
    assert!(
        matches!(&e, Err(FilesError::Sql { code: 1, message }) if message.contains("no such table")),
        "{e:?}"
    );
    // Two statements in one transaction: the second fails, the first is rolled back.
    let e = s.sql("UPDATE t SET v = 'no'; SELEC 1", vec![], None);
    assert!(
        matches!(&e, Err(FilesError::Sql { code: 1, message }) if message.contains("syntax error")),
        "{e:?}"
    );
    // Read from the daemon, read-only: all types.
    let op = s.fc.sql(DB, "SELECT id, v, n, x'00ff', NULL FROM t ORDER BY id", vec![], None, true);
    let Outcome::Sql(r) = s.ok(op) else { panic!() };
    assert_eq!(r.columns, ["id", "v", "n", "x'00ff'", "NULL"]);
    assert_eq!(
        r.rows,
        [
            vec![
                SqlValue::Int(1),
                SqlValue::Text("modificato dall'host".into()),
                SqlValue::Real(1.5),
                SqlValue::Blob(vec![0, 0xff]),
                SqlValue::Null
            ],
            vec![
                SqlValue::Int(3),
                SqlValue::Text("tre".into()),
                SqlValue::Real(-0.25),
                SqlValue::Blob(vec![0, 0xff]),
                SqlValue::Null
            ],
        ]
    );
    let op = s.fc.sql(DB, "DELETE FROM t", vec![], None, true);
    assert!(matches!(s.wait(op), Err(FilesError::Sql { code: 8, .. })), "read-only");
    let op = s.fc.sql("/tmp/app/databases/manca.db", "SELECT 1", vec![], None, true);
    assert_eq!(s.wait(op), Err(FilesError::Errno(2)));
    let op = s.fc.sql("/tmp/app/databases", "SELECT 1", vec![], None, true);
    assert_eq!(s.wait(op), Err(FilesError::Errno(21)));
    let out = s.app_sqlite("SELECT count(*), group_concat(v) FROM t");
    assert_eq!(out, "2|modificato dall'host,tre", "nothing changed after the errors");

    // ---- A rollback-journal database of another user -------------------------
    let out = s.command(
        "sqlite3 -batch -list /tmp/r.db 'CREATE TABLE k (a); INSERT INTO k VALUES (1);' && chown 1234:1234 /tmp/r.db && ls /tmp | grep r.db",
    );
    assert_eq!(out, "r.db");
    let op = s.fc.sql("/tmp/r.db", "UPDATE k SET a = ?1", vec![SqlValue::Int(2)], Some(1), false);
    let Outcome::Sql(r) = s.ok(op) else { panic!() };
    assert_eq!(r.changes, 1);
    let out = s.command(
        "sqlite3 -batch -list /tmp/r.db 'SELECT a FROM k'; ls /tmp | grep r.db; stat -c '%u %g' /tmp/r.db",
    );
    assert_eq!(out, "2\nr.db\n1234 1234", "no journal left");
    // The SQL runs with the owner's uid, not as root: in a folder
    // where only root writes the journal is not created and the write fails.
    s.command("mkdir /tmp/solo-root && sqlite3 /tmp/solo-root/x.db 'CREATE TABLE k (a)' && chown 1234:1234 /tmp/solo-root/x.db");
    let op = s.fc.sql("/tmp/solo-root/x.db", "INSERT INTO k VALUES (1)", vec![], Some(1), false);
    let e = s.wait(op);
    assert!(matches!(&e, Err(FilesError::Sql { code: 8, .. })), "SQLITE_READONLY expected: {e:?}");
    assert_eq!(
        s.command(
            "ls /tmp/solo-root | cat; sqlite3 -batch -list /tmp/solo-root/x.db 'SELECT count(*) FROM k'"
        ),
        "x.db\n0"
    );

    // ---- SharedPreferences --------------------------------------------------
    let prefs = "/tmp/app/shared_prefs/prefs.xml";
    let w = s.fc.write_file(prefs, PREFS.as_bytes(), 0o660);
    s.ok(w);
    s.command(&format!("chown 10057:10057 {prefs} && chmod 660 {prefs}"));
    let r = s.fc.read_file(prefs);
    let Outcome::Data { data, .. } = s.ok(r) else { panic!() };
    // The panel changes a value and adds one (tests/web: same
    // serialisation as the JS); here the result.
    let edited = String::from_utf8(data).unwrap().replace(
        "<int name=\"avvii\" value=\"3\" />",
        "<int name=\"avvii\" value=\"42\" />\n    <float name=\"scala\" value=\"1.5\" />",
    );
    let w = s.fc.write_file(prefs, edited.as_bytes(), 0o600);
    let Outcome::Written(st) = s.ok(w) else { panic!() };
    assert_eq!((st.mode, st.uid, st.gid), (0o100660, 10057, 10057));
    let out = s.command(&format!("grep -o 'avvii\" value=\"[0-9]*' {prefs}; grep -c scala {prefs}"));
    assert_eq!(out, "avvii\" value=\"42\n1");

    // ---- Non-UTF-8 names -------------------------------------------------------
    s.command("mkdir /tmp/n && printf 'dal-guest' > \"/tmp/n/$(printf 'a\\377b')\"");
    let l = s.fc.list("/tmp/n");
    let Outcome::List(entries) = s.ok(l) else { panic!() };
    assert_eq!(entries.len(), 1);
    assert_eq!((entries[0].name.as_slice(), entries[0].stat.kind), (&b"a\xffb"[..], Kind::File));
    let r = s.fc.read_file(b"/tmp/n/a\xffb");
    assert_eq!(s.ok(r), Outcome::Data { size: 9, data: b"dal-guest".to_vec() });
    let w = s.fc.write_file(b"/tmp/n/a\xffb", b"dall'host", 0o644);
    s.ok(w);
    let m = s.fc.rename(b"/tmp/n/a\xffb", b"/tmp/n/c\xfe\xc3\xa0");
    assert_eq!(s.ok(m), Outcome::Done);
    let out = s.command("cd /tmp/n && printf '%s\\n' * | od -An -tx1; cat *; cd /");
    assert_eq!(out.split_whitespace().collect::<Vec<_>>().join(" "), "63 fe c3 a0 0a dall'host");

    s.m.input(Input::Console(b"poweroff -f\n".to_vec()));
    let limit = s.m.steps + PHASE_BUDGET;
    let stop = loop {
        let st = s.quantum();
        if st != Stop::Budget || s.m.steps >= limit {
            break st;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "{}", s.tail());
    s.log.extend(s.m.console_output());
    (s.log, s.m.steps, s.seen)
}

#[test]
fn modifica_sqlite_preferenze_e_nomi_nel_guest() {
    if cfg!(debug_assertions) {
        skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "file manager on the guest kernel only in release");
        return;
    }
    let Some((image, initrd)) = guest_kernel() else {
        skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
        );
        return;
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let a = session(&image, &initrd);
    std::fs::write(repo_root().join("target/guest-kernel/vetro-files-modifica.log"), &a.0).unwrap();
    eprintln!("file manager editing: {} instructions, {} host steps", a.1, a.2.len());
    let b = session(&image, &initrd);
    assert_eq!(a.1, b.1, "instructions differ between two identical runs");
    assert!(a.0 == b.0, "logs differ between two identical runs");
    assert_eq!(a.2, b.2, "responses differ between two identical runs");
}
