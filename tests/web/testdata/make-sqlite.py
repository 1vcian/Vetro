#!/usr/bin/env python3
# Generates tests/web/testdata/prova.sqlite, the test database of the file
# manager's SQLite reader (web/app/sqlite.mjs, M8). The file is in the
# repository: this script is only needed to regenerate it (python3 with the
# standard library's sqlite3 module).
#
# Inside: 1024-byte pages (multi-level b-tree), a table with a rowid
# alias and all value types (NULL, small and 8-byte integers,
# reals, 0 and 1, text, BLOB), 600 rows (interior pages), a 5000-byte
# text (overflow pages), a WITHOUT ROWID table with a composite
# key, a table without a rowid alias and with quoted names.
#
# Then wal.sqlite and wal.sqlite-wal (ADR 0021): a WAL database copied with
# the connection still open, hence with transactions only in the -wal (a
# row changed, one added, one removed, a new table that grows
# the file) and at the end a damaged frame, which doesn't count.
import os
import shutil
import sqlite3

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "prova.sqlite")
if os.path.exists(out):
    os.remove(out)
db = sqlite3.connect(out)
db.execute("PRAGMA page_size = 1024")
db.execute("PRAGMA journal_mode = DELETE")
db.execute(
    "CREATE TABLE valori (id INTEGER PRIMARY KEY, nome TEXT NOT NULL, n INTEGER, x REAL, dati BLOB)"
)
db.execute("INSERT INTO valori VALUES (1, 'nullo', NULL, NULL, NULL)")
db.execute("INSERT INTO valori VALUES (2, 'zero e uno', 0, 1.5, x'00ff')")
db.execute("INSERT INTO valori VALUES (3, 'uno', 1, -2.25, x'')")
db.execute("INSERT INTO valori VALUES (4, 'grande', 9007199254740993, 0.1, NULL)")
db.execute("INSERT INTO valori VALUES (5, 'negativo', -300000, 1e300, NULL)")
db.execute("INSERT INTO valori VALUES (6, 'àèìòù €', 70000, NULL, NULL)")
db.execute("INSERT INTO valori VALUES (7, ?, 7, NULL, NULL)", ("L" * 5000,))
db.execute('CREATE TABLE "molte righe" ("chiave" TEXT, [valore] INTEGER)')
db.executemany('INSERT INTO "molte righe" VALUES (?, ?)', [(f"riga-{i:04d}", i * i) for i in range(600)])
db.execute(
    "CREATE TABLE prefs (utente TEXT, chiave TEXT, valore TEXT, PRIMARY KEY (utente, chiave)) WITHOUT ROWID"
)
db.executemany(
    "INSERT INTO prefs VALUES (?, ?, ?)",
    [("anna", "tema", "scuro"), ("bruno", "lingua", "it"), ("anna", "lingua", "en")],
)
db.commit()
db.close()
print(out, os.path.getsize(out), "byte")

wal = os.path.join(os.path.dirname(out), "wal.sqlite")
for f in (wal, wal + "-wal", wal + "-shm"):
    if os.path.exists(f):
        os.remove(f)
db = sqlite3.connect(wal, isolation_level=None)
db.execute("PRAGMA page_size = 1024")
db.execute("PRAGMA journal_mode = WAL")
db.execute("PRAGMA wal_autocheckpoint = 0")
db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
db.executemany("INSERT INTO t VALUES (?, ?)", [(i, f"base-{i}") for i in range(1, 6)])
db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
db.execute("UPDATE t SET v = 'dal-wal' WHERE id = 2")
db.execute("INSERT INTO t VALUES (6, 'nuova')")
db.execute("DELETE FROM t WHERE id = 5")
db.execute("CREATE TABLE altra (x)")
db.executemany("INSERT INTO altra VALUES (?)", [("A" * 900,) for _ in range(4)])
# Copy with the connection open: on close SQLite would do the
# checkpoint and remove the -wal.
shutil.copyfile(wal, wal + ".copia")
shutil.copyfile(wal + "-wal", wal + "-wal.copia")
db.close()
os.replace(wal + ".copia", wal)
os.replace(wal + "-wal.copia", wal + "-wal")
if os.path.exists(wal + "-shm"):
    os.remove(wal + "-shm")
with open(wal + "-wal", "r+b") as f:
    data = f.read()
    frame = bytearray(data[32 : 32 + 24 + 1024])
    # A "commit" frame at the end with the right salts and checksums that don't
    # add up (an interrupted write): the reader must ignore it.
    frame[4:8] = (99).to_bytes(4, "big")
    f.write(frame)
print(wal, os.path.getsize(wal), "byte +", os.path.getsize(wal + "-wal"), "byte di WAL")
