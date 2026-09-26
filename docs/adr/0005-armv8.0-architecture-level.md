# ADR 0005 — Livello architetturale di partenza: ARMv8.0-A (Cortex-A53)

- Stato: accettata (M1, 2026-09-24)

## Contesto
Il confronto con QEMU ha senso solo se QEMU emula la stessa CPU che
dichiariamo al guest. Con `-cpu max` QEMU implementa decine di estensioni
(LSE, PAuth, MTE, SVE, CSSC...): istruzioni che per noi sarebbero UNDEFINED
risulterebbero valide nell'oracolo, e viceversa.

## Decisione
- Vetro parte da **ARMv8.0-A con le estensioni della Cortex-A53**: CRC32,
  AdvSIMD/FP, AES/SHA1/SHA2/PMULL. Nessun LSE, PAuth, MTE, SVE.
- L'oracolo gira sempre con `-cpu cortex-a53` (variabile
  `VETRO_QEMU_CPU` per cambiarlo in esperimenti).
- Gli ID register che esporremo (M2/M3) descrivono esattamente questo
  livello, così il guest non usa estensioni che non abbiamo.
- Nel decoder si distinguono due esiti: `Undefined` (l'architettura dice
  UNDEFINED a questo livello) e `Unimplemented` (istruzione valida che non
  abbiamo ancora scritto, es. SIMD/FP in M1). Un `Unimplemented` non si
  confonde mai con un comportamento architetturale.

## Conseguenze
Android 15 arm64 gira su ARMv8.0 (le librerie rilevano LSE a runtime). Le
estensioni si aggiungono una alla volta, ciascuna con un ADR che alza il
modello QEMU dell'oracolo (es. `neoverse-n1` per v8.2).
