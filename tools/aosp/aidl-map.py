#!/usr/bin/env python3
"""Mappa AIDL dell'immagine AOSP di Vetro (M8, decoder Binder).

Dai jar di /system/framework estratti dall'immagine (tools/aosp/aidl-map.sh)
legge con `dexdump -f` le costanti che AIDL genera negli stub:
`TRANSACTION_<metodo>` di `<interfaccia>$Stub` (codice -> metodo) e, per le
interfacce scritte a mano come android.content.IContentProvider, le costanti
`<NOME>_TRANSACTION` dell'interfaccia. Il descrittore è il nome della classe
dell'interfaccia (quello che writeInterfaceToken mette in testa al Parcel).

Uscita, una riga per metodo, ordinata: `descrittore<TAB>codice<TAB>metodo`.
Uso: aidl-map.py DEXDUMP FILE.dex... > mappa.tsv
"""
import re
import subprocess
import sys

dexdump, dexes = sys.argv[1], sys.argv[2:]
field = re.compile(r"#\d+\s+: \(in L([^;]+);\)")
out = set()
for dex in dexes:
    text = subprocess.run([dexdump, "-f", dex], capture_output=True, text=True, errors="replace").stdout
    cls = name = None
    for line in text.splitlines():
        m = field.search(line)
        if m:
            cls, name = m.group(1).replace("/", "."), None
            continue
        s = line.strip()
        if s.startswith("name") and cls:
            name = s.split(":", 1)[1].strip().strip("'")
        elif s.startswith("value") and cls and name:
            v = s.split(":", 1)[1].strip()
            if not re.fullmatch(r"-?\d+", v):
                continue
            code = int(v)
            if cls.endswith("$Stub") and name.startswith("TRANSACTION_"):
                out.add((cls[: -len("$Stub")], code, name[len("TRANSACTION_"):]))
            elif cls == "android.content.IContentProvider" and name.endswith("_TRANSACTION"):
                # QUERY_TRANSACTION -> query, OPEN_ASSET_FILE_TRANSACTION -> openAssetFile
                parts = name[: -len("_TRANSACTION")].lower().split("_")
                out.add((cls, code, parts[0] + "".join(p.title() for p in parts[1:])))
            name = None
for desc, code, meth in sorted(out):
    print(f"{desc}\t{code}\t{meth}")
