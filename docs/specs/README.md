# Spec dei componenti

Una spec per componente: interfaccia pubblica, invarianti, dipendenze
ammesse, test che ne verificano il comportamento. Cambiare un'interfaccia
richiede prima un ADR in `docs/adr/`.

Prossime da scrivere: `cpu.md` (stato dei registri, decoder, interfaccia
memoria verso `vetro-mmu`) prima di iniziare M1.

Spec presenti: `cpu.md`, `mmu.md`, `platform.md`, `net.md`, `jit.md`,
`wasm.md`, `snapshot.md` (M6: formato e API degli snapshot, ADR 0015),
`analysis.md` (M7: analisi di rete, pcapng, HTTP, HAR, ADR 0016),
`android-boot.md` (M5: boot.img, vendor_boot, init_boot e bootconfig, ADR 0018),
`replay.md` (M10: ingressi dell'host, registrazione, replay e salto, ADR 0019),
`files.md` (M8: gestore dei file, demone `vetro-files` su vsock, protocollo e
client, ADR 0020).
