# ADR 0028 — L'immagine AOSP di Vetro nel browser: memoria, disco, adb, snapshot

- Stato: accettata (M5 e M6, 2026-09-26). Usa ADR 0014 e 0017 (dischi dal
  browser), 0015 (snapshot), 0018 (bootloader Android), 0022 (immagine
  AOSP), 0024 e 0026 (JIT). Dettagli: `docs/specs/wasm.md` ("L'immagine AOSP
  nell'app"), `docs/specs/net.md` (adb).

## Contesto
L'immagine AOSP 15 di Vetro arriva alla home sotto `vetro boot --jit`
(3 GiB, una CPU). Per l'uscita di M5 deve farlo nel browser, con i dischi
presi da R2, schermo e input, adb dalla pagina; per M6 dal secondo avvio
deve ripartire da uno snapshot. Quattro domande: quanta RAM può avere il
guest dentro wasm32 (4 GiB di memoria lineare in tutto); come arrivano i
dischi (le immagini pubblicate sono file sparsi di Android, il disco GPT è
di 15 GiB quasi tutti zeri); come parla la pagina con adbd senza socket;
dove e quando si salva lo stato.

## Misure (Node 22 / V8, Apple silicon, macchina condivisa)
- **QEMU, 3 GiB, a sys.boot_completed + 2 min** (adb, `/proc/meminfo`):
  MemFree 242 MiB, Cached 1972 MiB, AnonPages 423 MiB, Slab 157 MiB. Il
  guest "usa" quasi tutta la RAM, ma con la cache delle pagine: quella
  anonima è meno di mezzo GiB.
- **vetro-wasm con 3 GiB** (`tests/web/android.mjs --ram=3072`): prima di
  tutto non partiva: su wasm32 un `Vec<u8>` non può superare `isize::MAX`
  (2 GiB - 1), "capacity overflow". Con la RAM come regione presa con
  `memory.grow` (sotto) parte: sys.boot_completed a 788,6 s di guest in
  1516 s reali (25 min, JIT in V8), memoria lineare 3298 MiB a 30 s,
  3632 MiB a zygote, 3887 MiB alla fine dell'avvio (RAM 3072 + copy-on-write
  dei dischi 330 MiB + blocchi del disco 64 MiB + JIT e resto); RSS del
  processo 0,4–1,1 GiB (le pagine mai toccate non occupano memoria). Lo
  **snapshot fallisce**: nel salvare il copy-on-write l'allocazione del
  buffer supera i 4 GiB (`handle_alloc_error` in `vetro_snapshot::compress`).
- **vetro-wasm con 2 GiB**: sys.boot_completed a 759,7 s di guest in
  1492 s reali; memoria lineare 2274 MiB a 40 s, 2862 MiB alla fine
  dell'avvio (RAM 2048 + copy-on-write 330 + blocchi 64 + JIT e resto). Lo
  snapshot con `Machine::save` **fallisce anche qui**: il `Vec` del file
  cresce raddoppiando (e una riallocazione vuole vecchio + nuovo). Col
  salvataggio a pezzi (sotto) riesce: 1376 MiB in 36 s (scrittura su disco
  compresa), memoria lineare al massimo 3209 MiB; ripristino su una macchina
  nuova in 8,6–9,8 s (Node, lettura del file compresa).
- **Codice del JIT**: dopo ~30 min V8 chiude il processo ("Exceeding
  maximum wasm committed code space", 4 GiB di codice compilato): il motore
  JS non liberava mai i moduli (sotto).
- memory64 non è stato provato: il JIT genera moduli con memoria a 32 bit
  (`vetro-jit`, ADR 0012/0024) e la TLB software punta dentro la memoria
  lineare; cambiare vorrebbe dire un altro backend del JIT e controlli dei
  limiti espliciti (niente pagine di guardia), un costo che le misure qui
  sopra non giustificano.

## Decisione

### RAM: 2 GiB nel browser, regione oltre 2 GiB possibile
- Nell'app la RAM del guest con AOSP è **2048 MiB** (`ANDROID_RAM_MIB`,
  modificabile con `?ram=`). Il guest ci sta largo (sotto mezzo GiB di
  memoria anonima alla fine dell'avvio con 3 GiB) e restano quasi 2 GiB di
  memoria lineare per il copy-on-write (330 MiB dopo il primo avvio), la
  cache dei blocchi, il JIT e soprattutto lo snapshot, che si costruisce in
  memoria (centinaia di MiB) e al ripristino si legge in memoria.
  `androidboot.ddr_size=3072MB` resta nel bootconfig dell'immagine (non si
  può ridefinire dal bootloader senza che il kernel scarti il blocco):
  serve solo al dimensionamento di alcune proprietà.
- `vetro_machine::board::Ram` diventa un blocco contiguo che su wasm32,
  oltre `isize::MAX`, è una **regione presa con `memory.grow` fuori
  dall'allocatore** e si legge solo a pezzi (hash, confronti, snapshot
  pagina per pagina con lo stesso formato di `vetro_snapshot::compress`).
  Contigua come prima: la TLB software del JIT (`SysPhys::ram_region`) non
  cambia. La regione di una macchina distrutta si riusa (azzerata). Serve a
  chi vuole 3 GiB senza snapshot, e ha permesso la misura.
- **Snapshot a pezzi**: `Machine::save_stream` (vetro-wasm
  `vetro_snapshot_save_stream`, import `vetro_host.snapshot_write`) manda il
  file al JS a pezzi da 1 MiB, che il Worker scrive in OPFS man mano; in
  memoria resta solo la parte prima della RAM (dispositivi e copy-on-write,
  con il buffer dimensionato in anticipo). La RAM si comprime due volte: la
  lunghezza del contenuto entra nell'hash dell'intestazione prima di tutto
  il resto, e il formato del file non cambia (ADR 0015). Il ripristino legge
  il file da OPFS direttamente in un buffer della memoria del modulo
  (`snapshotRestoreWith`). `Machine::save` scrive l'intestazione sul posto.
- **Limite del codice del JIT in V8** (`web/node/jit-engine.mjs`): al più
  96 MiB di moduli fra un azzeramento e l'altro, poi `compile` rifiuta e
  vetro-jit azzera il motore (lo stesso percorso della tabella piena);
  all'azzeramento il motore svuota le voci di `__indirect_function_table`
  date a Rust, che tenevano vivo il dispatcher, la sua tabella e quindi
  tutti i blocchi. Nessun cambiamento in vetro-jit.
- Scartate: "RAM a pagine allocate su richiesta" (le pagine mai toccate già
  non occupano memoria fisica in V8: il limite è lo spazio d'indirizzi, e la
  cache delle pagine del guest lo riempie comunque; più il JIT vorrebbe la
  RAM contigua), memory64 (sopra).

### Dischi: la mappa del disco sui file sparsi già pubblicati
- `tools/aosp/web-disk.mjs` scrive `web/disk.json` (dimensione ed
  estensioni `[offset nel disco, lunghezza, file, offset nel file]`, con
  riempimenti e buchi) e `web/disk-head.bin` (GPT e metadata, 364 KiB):
  super e userdata puntano dentro `super.img` e `userdata.img` sparsi,
  pezzo per pezzo (222 e 22 pezzi, 160 estensioni in tutto). Verificata
  byte per byte contro `target/aosp/disk.img` (15,07 GiB). Pubblicata con
  `tools/aosp/upload-web.sh` accanto alla versione (`aosp/<versione>/web/`):
  i file della versione non cambiano, niente disco da 15 GiB su R2.
- `LayoutSource` (web/node/disk.mjs) legge i byte con HTTP Range dai file
  della mappa; la chiave (cache OPFS dei blocchi, snapshot) è URL della
  mappa + SHA-256 del suo testo. Nel Worker: blocchi da 1 MiB, 64 in
  memoria, il resto nella cache OPFS.
- Le immagini di avvio (boot, vendor_boot, init_boot: 136 MiB, quasi tutti
  zeri) si scaricano solo per un avvio da zero, verificate con lo sha256 del
  manifest e tenute in OPFS; la chiave degli snapshot usa gli sha256 del
  manifest, così un ripristino non scarica niente.

### Avvio da boot.img in vetro-wasm (ABI 12)
`vetro_load_android` passa le immagini al bootloader di
`vetro_machine::android` (ADR 0018), stessi parametri di `tools/aosp/vetro.sh`
(`nokaslr`). Provato su un'immagine costruita da `mkbootimg.py` intorno al
kernel M3 con 3 GiB: istruzioni e log uguali al riferimento nativo.

### adb: client in JS sopra GuestSocket
`web/node/adb.mjs` parla il protocollo di adb sulla connessione TCP verso la
porta 5555 del guest (inoltro di porte, ABI 5): CNXN, AUTH (chiave RSA di
WebCrypto, pronta per una build user), `shell,v2,raw:`, `sync:` per push,
install = push + `pm install -r`, devices. Nel Worker si collega dopo
sys.boot_completed; le richieste della pagina sono ingressi (in `inputLog`
e nella timeline). APK trascinato: `web/node/apk.mjs` legge il manifesto
binario (pacchetto, attività principale), poi install e `am start -W -n`.
Scartato il ponte WebSocket verso un adb dell'host: servirebbe un processo
fuori dal browser.

### Bootloader: le righe del vendor_boot si possono sostituire (modifica all'ADR 0018)
Un `androidboot.*` dei parametri con la stessa chiave di una riga della
sezione bootconfig del vendor_boot la sostituisce al suo posto, invece di
ripeterla (il kernel scarterebbe tutto il blocco). Serve per
`androidboot.hardware.hwcomposer.display_framebuffer_format=bgra`
(`ANDROID_PARAMS`): l'immagine dice `rgba`, ma il driver virtio-gpu del
kernel crea i buffer dumb dello scanout sempre XRGB8888 (in memoria B, G,
R, X), e l'app di prova blu (0x1565c0) arrivava allo schermo come
(192, 101, 21). La correzione giusta è nell'immagine (ADR 0022, prossima
build); nel frattempo il bootloader la applica.

### Fasi dell'avvio
Dalla console (`web/node/android.mjs`): kernel, init prima e seconda fase,
zygote, surfaceflinger, system_server (primo messaggio di init con
"(system_server)"), avvio finito (`sys-boot-completed-set`, l'evento di
init su `sys.boot_completed=1`: la riga "processing action
(sys.boot_completed=1…)" non compare in kmsg). Poi la **home**, che non è
nella console: dopo sys.boot_completed resta a lungo FallbackHome ("Phone
is starting", 600 s di guest in più con 2 GiB); il Worker chiede ad adb
`dumpsys window | grep -m1 mCurrentFocus` ogni 5 s di guest finché è il
launcher (`mResumedActivity` di `dumpsys activity` in Android 15 non c'è
più). Dopo il collegamento il Worker tiene acceso lo schermo (`svc power
stayon true`).

### Snapshot di Android
- Si salva 5 s di guest dopo che la home è a schermo (o 3000 s di guest
  dopo sys.boot_completed se la home non si vede), dopo l'installazione di
  un APK e col pulsante; il riposo della console (ADR 0017) non vale per
  Android.
- **Lo snapshot è l'unità di persistenza**: contiene già il copy-on-write
  dei dischi (ADR 0015), quindi con Android niente overlay separato in OPFS
  (330 MiB in più da scrivere a ogni salvataggio e da rileggere a ogni
  avvio). Alla sessione successiva si riparte dall'ultimo snapshot; le
  scritture fatte dopo si perdono; senza snapshot si rifà il primo avvio.
- Snapshot pre-costruito su R2: vedi "Conseguenze".

## Conseguenze
- La home di AOSP arriva nell'app in Chrome; tempi e dimensioni in
  `docs/progress/M5.md` e `M6.md`.
- Un guest da 3 GiB nel browser si avvia ma non si può salvare: il limite
  vero è la memoria lineare, non la RAM del Mac.
- Uno snapshot pre-costruito scaricato da R2 al primo avvio eviterebbe i
  25+ minuti del primo avvio, ma: deve essere della stessa versione del
  formato degli snapshot, della stessa configurazione della macchina e dello
  stesso vetro-wasm di chi lo scarica (una versione di Vetro nuova lo
  invalida), e pesa quanto uno snapshot locale (vedi le misure) — compresso
  con gzip il guadagno misurato è in `docs/progress/M6.md`. Da fare quando le
  versioni di vetro-wasm pubblicate saranno versionate insieme agli
  snapshot (un artefatto per coppia immagine/vetro-wasm).
- `androidboot.ddr_size` e le proprietà di dalvik restano quelle per 3 GiB:
  se lmkd si rivelasse troppo aggressivo con 2 GiB, un bootconfig per il
  browser nell'immagine (ADR 0022) è la strada.
