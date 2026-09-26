#!/usr/bin/env python3
# Estrae una partizione logica da super.img non sparso (metadati LP di
# Android: geometria, intestazione, tabelle partizioni ed estensioni).
# Uso: lp-extract.py super.raw NOME uscita.img  (senza NOME: elenca)
import struct, sys
f = open(sys.argv[1], 'rb')
f.seek(4096 + 4096 * 2)  # geometria (4096) + copia; metadati primari dopo 2 geometrie
hdr = f.read(256)
magic, major, minor, hsize = struct.unpack_from('<IHHI', hdr, 0)
assert magic == 0x414C5030, hex(magic)
# tabelle: partitions, extents, groups (offset, num, entry_size) dopo checksum
tabs_off = 4 + 2 + 2 + 4 + 32 + 4 + 32
parts = struct.unpack_from('<III', hdr, tabs_off)
exts = struct.unpack_from('<III', hdr, tabs_off + 12)
f.seek(4096 + 4096 * 2 + hsize)
body = f.read(parts[0] + parts[1] * parts[2] + exts[1] * exts[2] + 65536)
out = {}
for i in range(parts[1]):
    e = body[parts[0] + i * parts[2]: parts[0] + (i + 1) * parts[2]]
    name = e[:36].split(b'\0')[0].decode()
    attrs, first, num, grp = struct.unpack_from('<IIII', e, 36)
    ext = []
    for j in range(first, first + num):
        x = body[exts[0] + j * exts[2]: exts[0] + (j + 1) * exts[2]]
        nsec, ttype, tdata, tdev = struct.unpack_from('<QIQI', x, 0)
        ext.append((nsec, ttype, tdata))
    out[name] = ext
    print(name, ext)
if len(sys.argv) > 3:
    name, dst = sys.argv[2], sys.argv[3]
    with open(dst, 'wb') as o:
        for nsec, ttype, tdata in out[name]:
            f.seek(tdata * 512)
            left = nsec * 512
            while left:
                b = f.read(min(left, 1 << 24)); o.write(b); left -= len(b)
