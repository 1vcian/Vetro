#!/usr/bin/env python3
"""AIDL map of the Vetro AOSP image (M8, Binder decoder).

From the /system/framework jars extracted from the image (tools/aosp/aidl-map.sh)
reads with `dexdump -f` the constants that AIDL generates in the stubs:
`TRANSACTION_<method>` of `<interface>$Stub` (code -> method) and, for
hand-written interfaces like android.content.IContentProvider, the interface's
`<NAME>_TRANSACTION` constants. The descriptor is the interface's class
name (the one writeInterfaceToken puts at the head of the Parcel).

Output, one line per method, sorted: `descriptor<TAB>code<TAB>method`.
Usage: aidl-map.py DEXDUMP FILE.dex... > map.tsv
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
