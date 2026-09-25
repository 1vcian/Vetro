# M5 — immagini Android per Vetro (ricerca, 2026-09-25)

Base per l'ADR di M5. Verificato con fonti ufficiali e con una scansione
dell'immagine dell'emulatore Android 15 arm64 "default": 1.681 ELF
disassemblati; `linker64` e `toybox` eseguiti sotto `qemu-aarch64 -cpu
cortex-a53`.

## Conclusioni
- **ISA: nessuna estensione oltre ARMv8.0 + CRC32 + crypto**, che Vetro ha già
  (ADR 0005). Le fonti:
  - Soong compila con `-march=armv8-a -mcpu=cortex-a53`;
  - ART assume CRC32 per le varianti generic e cortex-a53;
  - LSE, SHA512/SHA3, MTE, DotProd e SVE compaiono solo con selezione a
    runtime;
  - PAC/BTI stanno nello spazio HINT, cioè NOP su v8.0.

  Vincoli:
  - HWCAP senza capacità che Vetro non ha;
  - `dalvik.vm.isa.arm64.variant` = `generic` o `cortex-a53`: con a55 o
    successive, il JIT di ART genererebbe LSE e FP16.

  Resta da verificare, con un avvio completo sotto `qemu-system-aarch64 -M virt
  -cpu cortex-a53`: circa 361 `sdot` attribuiti a `libinput.so`, quasi certamente
  dati.
- **Immagini pre-costruite:**
  - **Emulatore (`sdk_phone64_arm64`, API 35)**
    - virtio-mmio con la mappa della virt, avvio diretto.
    - Grafica solo via gfxstream dell'host e dispositivi goldfish.
    - Licenza SDK: non ridistribuibile. Utile solo come prova locale.
  - **Cuttlefish `aosp_cf_arm64_only_phone`**
    - armv8-a/cortex-a53, solo 64 bit; grafica SwiftShader su virtio-gpu 2D
      (adatta).
    - Dispositivi virtio-pci, e `virtio_mmio.ko` solo nella seconda fase di
      init. Senza PCI non trova i dischi.
    - u-boot, boot.img v4 con vendor_boot e init_boot, AVB.
  - **GSI:** solo system, non basta.
- **microG**
  - GmsCore, Apache 2.0, come app privilegiata prebuilt.
  - Richiede lo spoofing della firma, cioè una patch a `frameworks/base`
    (modello LineageOS, limitato a microG).
  - Quindi serve una build nostra.
- **Build di AOSP 15**
  - Solo su host Linux x86_64: macOS non è supportato da Android 11, e un host
    Linux arm64 non è supportato.
  - Almeno 400 GB di disco e 64 GB di RAM; tempi ufficiali: circa 6 ore su 6
    core, circa 40 minuti su 72.
  - Si costruisce da `android-latest-release` (aosp-main è in sola lettura dal
    27/03/2025).
- **Kernel:** GKI android15-6.6 prebuilt, con i sorgenti pubblicati per la GPL.
  Il 6.18 minimale di ADR 0008 non basta: mancano binder, eBPF, dm-verity e
  altro.

## Raccomandazione
1. `guest/aosp` = fork di `vsoc_arm64_only`, costruito sulla macchina Linux
   x86_64 dedicata. Modifiche:
   - `virtio_mmio.ko` nella prima fase, `boot_devices` su mmio;
   - SwiftShader + drm_hwcomposer + minigbm;
   - adbd su TCP;
   - vbmeta disattivato (userdebug);
   - variante ISA di ART cortex-a53;
   - microG con lo spoofing limitato.
2. Prima di costruire:
   - avviare l'immagine dell'emulatore API 35 (solo in locale) e la Cuttlefish
     rimpacchettata sotto `qemu-system-aarch64 -M virt -cpu cortex-a53 -smp 1`,
     per confermare ISA e dispositivi;
   - estendere il caricatore (ADR 0008) a boot.img v4 + vendor_boot +
     bootconfig.
3. Valutare con un ADR un host PCIe ECAM (virtio-pci, INTx): renderebbe
   avviabili le immagini Cuttlefish senza modificarle.

## Rischi
- **Dispositivi:** PCI contro mmio nella prima fase di init; la catena
  bootconfig/AVB/boot_devices è fragile.
- **Risorse:** RAM del guest di 2–3 GB (memory64 nel browser); primo avvio con
  dexopt lungo.
- **Codice automodificante:** eBPF JIT e ART JIT, gestiti dall'invalidazione di
  M4.
- **Legali:** l'immagine SDK non si ridistribuisce; i sorgenti GKI vanno
  pubblicati.

## Non verificato
- Dimensioni e termini degli artefatti su ci.android.com.
- Avvio con una sola CPU.
- VINTF con kernel più recenti di 6.6.
- ART/dex2oat sotto cortex-a53.

Fonti: link nel rapporto originale, tra cui source.android.com
(Cuttlefish, 16KB, CDD 15, requirements), android.googlesource.com
(`device/google/cuttlefish`, `device/generic/goldfish`, `art`,
`build/soong`), github.com/microg/GmsCore.
