# Vetro — piano e stato

Fonte: piano originale plan.docx (24/09/2026, non versionato). Questo file è la copia di lavoro: le
milestone si segnano complete **solo** quando il comando di uscita passa in CI.

## Visione in breve

Android 15 (API 35) arm64 da AOSP, con microG, emulato nel browser da una CPU
ARM64 nostra (interprete + JIT verso WASM), su una piattaforma copia della
"virt" di QEMU. L'analisi (syscall, Binder, TLS, ART) avviene dall'emulatore,
invisibile alle app. Primo target Chrome/Edge desktop.

### Criteri di successo della 1.0

- Home in meno di 15 s da snapshot in cache, su Chrome o Edge desktop.
- APK trascinato: si installa e si usa con mouse e tastiera.
- Set di riferimento di 30 app (20 F-Droid, 10 note senza Play Integrity):
  avvio, login, navigazione.
- Ogni richiesta di rete in chiaro, attribuita a processo e libreria, legata
  sulla timeline all'azione utente.
- Ogni accesso a dati sensibili registrato decodificando Binder.
- Export in HAR, pcap e JSON.

### Fuori perimetro 1.0

Play Store / GMS originali / Play Integrity; giochi 3D pesanti; iPhone e
Safari mobile; qualsiasi componente cloud obbligatorio.

## Stato

| Milestone | Stato | Comando di uscita |
|---|---|---|
| M0 Impalcatura e oracolo | **completata** (CI verde, 2026-09-24) | `VETRO_REQUIRE_ORACLE=1 tools/ci.sh` |
| M1 Interprete CPU AArch64 | **completata** (CI verde, 2026-09-24) | test per istruzione + 200 programmi casuali uguali a qemu-aarch64 (ADR 0006) |
| M2 Syscall Linux utente | **completata** (CI verde, 2026-09-25) | 355 test LTP, BusyBox e RISU uguali all'oracolo nel job linux (ADR 0010) |
| M3 Modalità sistema e kernel | **completata** (CI verde, 2026-09-25) | Linux 6.18 fino alla shell con log uguale a QEMU; 95 kselftest con esiti uguali (ADR 0011) |
| M4 JIT verso WASM | **in corso** (ADR 0012, 0013; criterio verde in locale, manca la CI) | test M1/M2 col JIT, parità interprete-JIT |
| M5 Avvio di Android | — | home in Chrome, adb install di un APK |
| M6 Snapshot e installazione | — | home < 15 s da snapshot, APK trascinato |
| M7 Rete e timeline | — | 10 app con HTTPS in chiaro e legato all'azione, HAR riapribile |
| M8 Binder e privacy | — | app di test: ogni accesso rilevato, identificativo esca tracciato |
| M9 Tracciamento codice e scripting | — | hook su metodo, dex dinamico salvato, script Frida adattato |
| M10 Record & replay, 1.0 | — | replay identico, 30 app, criteri 1.0 |

## Milestone

### M0 — Impalcatura e oracolo
- **Obiettivo:** repository, CI e confronto con QEMU funzionante da subito.
- **Consegne:** struttura del repo, CLAUDE.md, CI nativa e WASM, QEMU e RISU
  richiamabili dai test.
- **Uscita:** `cargo test` verde; un test che lancia QEMU e ne legge l'uscita
  passa in CI.

### M1 — Interprete CPU AArch64
- **Obiettivo:** eseguire codice ARM64 utente, lento ma corretto.
- **Consegne:** decoder e interprete per interi, memoria, salti, condizioni;
  caricatore di ELF statici; il runner CLI esegue un binario.
- **Uscita:** suite per istruzione verde; su almeno 200 programmi casuali RISU
  i registri finali coincidono con qemu-aarch64.

### M2 — Syscall Linux utente
- **Obiettivo:** veri programmi Linux arm64 in user mode.
- **Consegne:** gestione SVC, syscall di base (file, memoria, thread, clock)
  mappate sull'host; primo tracer di syscall.
- **Uscita:** selezione LTP user mode verde; BusyBox arm64 statica esegue i
  comandi principali; log delle syscall corretto.

### M3 — Modalità sistema e avvio del kernel
- **Obiettivo:** kernel Linux arm64 fino alla shell.
- **Consegne:** EL0/EL1, MMU stage 1, GICv3, timer, UART, virtio-blk e
  virtio-net minimi, device tree, caricatore del kernel.
- **Uscita:** kernel con initramfs alla shell con avvio scriptato; selezione
  kselftest verde nel guest.

### M4 — JIT verso WASM
- **Obiettivo:** prestazioni lavorabili.
- **Consegne:** traduzione a blocchi in moduli WASM, cache, invalidazione su
  codice modificato, ritorno all'interprete nei casi difficili.
- **Uscita:** test M1 e M2 verdi col JIT; avvio kernel sotto la soglia fissata
  in M3; nessuna differenza interprete-JIT sul set differenziale.

### M5 — Avvio di Android
- **Obiettivo:** home di Android nel browser.
- **Consegne:** immagini AOSP 15 arm64; disco a blocchi via HTTP Range con
  cache OPFS; virtio-gpu 2D su WebGPU; virtio-input; adb su canale virtio;
  sinkhole di rete.
- **Uscita:** home in Chrome; adb vede il dispositivo; adb install di un APK
  semplice, l'app si apre e reagisce al tocco.

### M6 — Snapshot e installazione
- **Consegne:** save/restore completo; snapshot di Android avviato;
  trascinamento APK; livello copy-on-write.
- **Uscita:** dal secondo avvio home < 15 s; APK trascinato si installa e si
  apre senza riga di comando.

### M7 — Analisi di rete e timeline
- **Consegne:** hook TLS su BoringSSL e Conscrypt; decodifica JSON, protobuf,
  form; ispettore di rete; timeline input→effetti; export HAR e pcap.
- **Uscita:** su 10 app del set ogni chiamata HTTPS è in chiaro e agganciata
  all'azione; l'HAR si riapre in un altro strumento.

### M8 — Binder e privacy
- **Consegne:** decoder Binder con mappatura AIDL; ispettore privacy; dati
  esca tracciati fino alla rete.
- **Uscita:** su un'app di test nostra ogni accesso previsto compare; un
  identificativo esca viene rilevato quando esce in rete.

### M9 — Tracciamento del codice e scripting
- **Consegne:** introspezione ART dall'emulatore; tracciamento nativo con
  simboli; rilevamento dex/librerie dinamici; API di scripting simile a Frida.
- **Uscita:** uno script aggancia un metodo e registra le chiamate; un dex
  caricato a runtime viene salvato; uno script Frida d'esempio gira con
  modifiche minime.

### M10 — Record & replay e 1.0
- **Consegne:** registrazione deterministica e replay; salto a un evento con
  memoria e registri di quel momento; profili dispositivo; report; docs utente.
- **Uscita:** replay identico con ritorno al momento esatto di una chiamata;
  30 app superano i flussi base; criteri 1.0 soddisfatti.

## Squadra di agenti

| Agente | Cartelle | Attivo in |
|---|---|---|
| Architetto | `docs/`, interfacce tra crate | tutte |
| CPU | `crates/vetro-cpu`, `crates/vetro-mmu`, `tests/isa` | M1–M4 |
| JIT | `crates/vetro-jit`, `tests/diff` | M4, poi mantenimento |
| Piattaforma | `crates/vetro-platform`, `guest/kernel`, `tests/boot` | M3, M5 |
| Guest | `guest/aosp`, `guest/image` | M5–M6 |
| Rete | `crates/vetro-net`, `relay/` | M3, M7 |
| Analisi | `crates/vetro-analysis`, `crates/vetro-snapshot` | M2, M7–M10 |
| Web | `web/` | M5–M10 |
| Qualità | `tests/`, `.github/`, `tools/` | tutte |

Fino a M4: un solo agente per sessione. Parallelismo da M5.

## Rischi principali

JIT con bug sottili (parità obbligatoria); boot AOSP bloccato da dispositivi
mancanti (BusyBox → kernel → Android); prestazioni (JIT presto, snapshot);
memoria del browser (memory64, RAM configurabile); build AOSP fragile
(macchina dedicata, artefatti versionati); rilevamento dell'emulatore
(profili credibili, introspezione invisibile); TLS proprietari (ricerca per
firme); nodi legali (solo AOSP + microG, sorgenti kernel pubblicati).
Dettaglio completo nel piano originale (plan.docx, non versionato).
