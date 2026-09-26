# ADR 0004 — Licenza PolyForm Noncommercial 1.0.0

- Stato: accettata (2026-09-24). Sostituisce la scelta Apache-2.0 del piano.

## Contesto
Il codice di Vetro non deve poter essere usato commercialmente da terzi.
Apache-2.0 (previsto dal piano) lo consente.

## Decisione
Il codice nostro è rilasciato sotto **PolyForm Noncommercial 1.0.0**
(`LICENSE.md`, SPDX `PolyForm-Noncommercial-1.0.0`). Il file `NOTICE`
contiene la riga `Required Notice:` che la licenza obbliga a propagare.
L'uso commerciale richiede un accordo scritto separato con l'autore.

## Conseguenze
- Vetro è *source-available*, non open source secondo la definizione OSI:
  va detto così nel README e negli annunci.
- Contributi esterni: per poter concedere in futuro licenze commerciali,
  servono un CLA o una clausola di licenza in ingresso nelle PR. Da definire
  prima di accettare la prima PR esterna.
- Componenti di terzi mantengono la loro licenza: kernel Linux GPL-2.0
  (sorgenti pubblicati con ogni immagine), AOSP e microG Apache-2.0. Le
  immagini guest si distribuiscono come aggregato, con le rispettive licenze.
