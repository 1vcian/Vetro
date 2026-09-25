# mkbootimg di AOSP (riferimento per i test)

Copia **non modificata** dello strumento con cui AOSP costruisce `boot.img`,
`vendor_boot.img` e `init_boot.img`. I test del caricatore di immagini
Android (`crates/vetro-machine/src/android`, `docs/specs/android-boot.md`)
costruiscono le immagini con questo script, così il formato non è quello
che crediamo noi ma quello che produce AOSP.

- Sorgente: `https://android.googlesource.com/platform/system/tools/mkbootimg`
- Commit: `d2bb0af5ba6d3198a3e99529c97eda1be0b5a093` (2 marzo 2025, ramo `main`)
- Licenza: Apache 2.0 (intestazione dei file).

| File | blob git | sha256 |
|---|---|---|
| `mkbootimg.py` | `ec2958179691a434df917cd1b6f196edaa80e31d` | `37d84b3d162e0bc62e36c1f4e1c63c85ea0caa9f29be023eb2f8efe006ad948c` |
| `gki/generate_gki_certificate.py` | `739c61b04a9dbd95cafa5196533e3a472c31f2d9` | `1bb1feec68a13da18d581aa2c631798f86f6bc10b55d587b2dd31446a0f8a203` |

`gki/generate_gki_certificate.py` serve solo perché `mkbootimg.py` lo importa
(la firma GKI 2.0, deprecata, non si usa).

Verifica: `git hash-object tools/mkbootimg/mkbootimg.py` deve dare il blob
della tabella, uguale a quello del commit indicato. Per aggiornare: scaricare
i due file dallo stesso commit (`?format=TEXT`, base64) e aggiornare la
tabella. Serve `python3` (solo libreria standard).
