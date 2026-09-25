# ADR 0021 — L'immagine AOSP 15 di Vetro: device vetro_arm64 derivato da Cuttlefish

- Stato: accettata (M5, 2026-09-26). Usa ADR 0005 (ISA), 0008 e 0018
  (avvio diretto e bootloader Android), 0020 (gestore dei file), 0004
  (licenze). Ricerca: `docs/research/m5-immagini-android.md`,
  `docs/research/m5-avvio-gki.md`. Dettagli: `docs/specs/guest-image.md`.

## Contesto
M5 chiede la home di Android nel browser. Le immagini pre-costruite non
vanno: quella dell'emulatore SDK non si ridistribuisce e compone solo via
gfxstream/goldfish (surfaceflinger abortisce sulla virt, sotto QEMU come
sotto Vetro); quelle di Cuttlefish cercano i dischi su virtio-pci; il GSI
è solo system. Serve un'immagine nostra, costruita da AOSP 15, che parta
sulla macchina virt di Vetro e di QEMU (GICv3, Cortex-A53, virtio-mmio,
niente PCI) con il bootloader di ADR 0018, e che porti microG.

## Decisione

### Base: prodotto nostro `vetro_arm64` sopra `vsoc_arm64_only`
- `guest/aosp/device/vetro/vetro_arm64` include il `BoardConfig.mk` di
  `device/google/cuttlefish/vsoc_arm64_only` e il vendor del telefono
  Cuttlefish (`shared/phone/device_vendor.mk`): HAL virtuali già pensati
  per una VM, SwiftShader, minigbm, HWC ranchu, KeyMint e Gatekeeper
  software. Cambia solo quello che la virt richiede. Un fork completo di
  Cuttlefish costerebbe ogni aggiornamento di AOSP; il device minimo da zero
  vorrebbe riscrivere decine di HAL.
- Solo 64 bit (`core_64_bit_only.mk`, armv8-a, `TARGET_CPU_VARIANT :=
  cortex-a53`): la CPU di Vetro non ha AArch32. Niente
  `packages/modules/Virtualization` (non c'è KVM).
- Tag AOSP `android-15.0.0_r36`, target `vetro_arm64-bp1a-userdebug`
  (`bp1a` è la release config del tag; `trunk_staging` accenderebbe flag in
  sviluppo). Build solo sulla VM Linux x86_64 dedicata (`tools/aosp`), mai
  in CI.
- Marchio: `PRODUCT_BRAND/MANUFACTURER := Vetro`, modello "Vetro arm64",
  numero di serie `VETRO00001`. Il prodotto non si presenta come Android né
  come Google; i nomi di pacchetto di microG (`com.google.android.gms`,
  `com.android.vending`) sono identificatori tecnici imposti dalla
  compatibilità, e le app si chiamano "microG Services" e "microG Companion".

### Kernel e prima fase
- Kernel GKI android15-6.6 prebuilt del tree (`kernel/prebuilts/6.6/arm64`,
  `6.6.57-android15-8-g8b48c9979699-ab12748506`), moduli di
  `kernel/prebuilts/common-modules/virtual-device/6.6/arm64`: gli stessi di
  Cuttlefish, non ricompilati.
- `virtio_mmio.ko` nel vendor_ramdisk, accanto a quelli che Cuttlefish ci
  mette già (virtio_blk, virtio_net, virtio-gpu, virtio_input,
  virtio_console, vmw_vsock_virtio_transport, virtio-rng, …): Cuttlefish lo
  carica solo nella seconda fase perché usa PCI; sulla virt senza di lui la
  prima fase non vede il disco. vsock è nel kernel.
- `androidboot.boot_devices` elenca tutti i 32 slot virtio-mmio della virt
  (`a000000.virtio_mmio` … `a003e00.virtio_mmio`): il disco si trova in
  qualunque slot finisca, con o senza vsock, sotto QEMU e sotto Vetro.
- I parametri che su Cuttlefish mette il launcher (`assemble_cvd`,
  `qemu_manager.cpp` con `--gpu_mode=guest_swiftshader`) stanno nella
  sezione bootconfig del vendor_boot (`BOARD_BOOTCONFIG`): né il bootloader
  di Vetro né QEMU devono aggiungere niente. Slot A, `force_normal_boot=1`,
  `verifiedbootstate=orange`.

### Dischi
- Un solo disco GPT (`tools/aosp/mkdisk.sh`): `misc`, `frp`, `metadata`
  (ext4 vuoto), `super` (partizioni logiche system, system_ext, product,
  system_dlkm, vendor, odm, vendor_dlkm, odm_dlkm) e `userdata` (f2fs vuoto
  della build; vold lo cifra al primo avvio). La prima fase trova le
  partizioni per nome GPT.
- fstab nostro (`fstab.vetro`, `androidboot.fstab_suffix=vetro`): le stesse
  voci di `fstab.cf.f2fs.hctr2` senza flag `avb`, senza condivisioni
  virtiofs e scheda SD.
- **AVB/vbmeta disattivato**: il fstab non chiede dm-verity, il bootloader di
  Vetro non legge vbmeta (ADR 0018), la build userdebug è "orange". La build
  produce comunque vbmeta firmato con le chiavi di test: non si usa.

### Grafica
- SwiftShader (Vulkan "pastel") con ANGLE per GLES 3.1, gralloc minigbm,
  HWC ranchu con composizione nel guest sul DRM di virtio-gpu 2D, gli stessi
  valori di Cuttlefish in `guest_swiftshader`. Niente gfxstream né virgl:
  la GPU di Vetro è 2D (su WebGPU nel browser). Densità 240, schermo quello
  di virtio-gpu (1280x800 per default).
- Senza animazione d'avvio e senza procedura guidata
  (`enable_bootanimation=0`, `setupwizard_mode=DISABLED`),
  `hw_timeout_multiplier=50` per una CPU emulata lenta.

### Sicurezza, adb, SELinux
- **adbd su TCP 5555 senza autorizzazione (`ro.adb.secure=0`) SOLO nella
  build di sviluppo userdebug**: Cuttlefish fa lo stesso. Una build per
  utenti deve togliere `ro.adb.secure=0` e usare le chiavi adb. L'host ci
  arriva con `--hostfwd=tcp::5555-:5555` (Vetro) o l'inoltro di QEMU.
- KeyMint e Gatekeeper software nel guest (`rust_nonsecure`, `nonsecure`):
  su Cuttlefish stanno sull'host, che qui non c'è.
- SELinux permissivo (`androidboot.selinux=permissive`, solo userdebug)
  finché la policy non copre i percorsi virtio-mmio: primo passo in
  `sepolicy/file_contexts` (sysfs di rete e blocchi come i percorsi PCI di
  Cuttlefish); obiettivo enforcing con i rifiuti registrati nei primi avvii.
- eth0 resta eth0 e la gestisce EthernetService con DHCP
  (`ro.vendor.disable_rename_eth0=1`): niente Wi-Fi simulato né OpenWRT
  dell'host di Cuttlefish.

### ART
- `dalvik.vm.isa.arm64.variant=cortex-a53` (da `TARGET_CPU_VARIANT`),
  controllato da `tools/aosp/fetch.sh` sugli artefatti: con a55 o varianti
  più nuove il JIT genererebbe LSE e FP16, che la CPU di Vetro non ha
  (ADR 0005). Niente ABI a 32 bit (controllato anche questo).
- Una CPU e 2–3 GiB di RAM (`androidboot.ddr_size=3072MB`, `vetro boot
  --mem=3072`, QEMU `-m 3G`).

### microG
- GmsCore e Companion, release ufficiale `v0.3.16.252432` di
  github.com/microg/GmsCore (Apache 2.0), scaricati sulla VM con sha256
  fissato (`guest/aosp/vendor/vetro/microg/microg.lock`), mai committati.
  App privilegiate prebuilt in `/product/priv-app`, con l'allowlist dei
  permessi privilegiati generata da `aapt2 dump permissions`, i permessi di
  runtime di default e l'esenzione dal risparmio energetico.
- Presigned: le due APK hanno targetSdk 29 e firma v1, la build scomprime dex
  e librerie JNI e la firma v1 resta valida (le app della partizione di
  sistema si verificano senza la protezione contro la rimozione di v2).
- **Spoofing della firma limitato a microG** in `frameworks/base`
  (`guest/aosp/patches/frameworks/base/0001-…patch`, applicata da
  `tools/aosp/remote/prepare.sh`): permesso `FAKE_PACKAGE_SIGNATURE`
  (`signature|privileged`, `@hide`), e in `ComputerEngine.generatePackageInfo`
  la firma del meta-data `fake-signature` sostituisce quella vera solo se il
  pacchetto è `com.google.android.gms` o `com.android.vending`, è firmato con
  il certificato di microG (SHA-256 `9bd06727…d14165`, verificato sulle
  release), chiede il permesso e l'ha ottenuto. Nessun'altra app può
  fingersi un'altra: è la variante "restricted" di LineageOS, più stretta.

### Gestore dei file
- Il demone `vetro-files` (ADR 0020) entra in `/vendor/bin`, compilato con
  bionic dallo stesso sorgente `guest/kernel/initramfs/vetro-files.c`
  (`tools/aosp/sync.sh` lo copia nel tree: nessuna copia in `guest/aosp`).
  Servizio di init `vetro_files` avviato a `post-fs-data` solo con
  `ro.debuggable=1`, `oneshot` (senza vsock esce e non si riavvia a ciclo).
- Dominio SELinux proprio `vetro_files` (`init_daemon_domain`, socket vsock
  permessi), **permissivo solo nelle build userdebug/eng**: deve leggere e
  scrivere i dati di ogni app come root, cosa che nessuna regola ristretta
  concede senza scontrarsi con i neverallow di AOSP. In una build user il
  dominio resta confinato e init non lo avvia.

### Artefatti e licenze
- `tools/aosp/fetch.sh` porta sul Mac `boot.img`, `vendor_boot.img`,
  `init_boot.img`, `super.img`, `userdata.img` con `SHA256SUMS` e
  `build-info.txt` (tag, BUILD_ID, kernel, commit dei progetti);
  `tools/aosp/upload.sh` li pubblica su Cloudflare R2 in
  `aosp/<tag>-<BUILD_ID>-<commit di Vetro>/` con `manifest.json` (sha256,
  dimensioni). Una versione pubblicata non cambia. Mai binari in git.
- Solo artefatti ridistribuibili: AOSP (Apache 2.0, con parti GPL/LGPL),
  microG (Apache 2.0). `tools/aosp/gpl-sources.sh` prepara i sorgenti
  esatti del kernel (kernel/common al commit della stringa di versione,
  moduli virtual-device al loro commit) e dei progetti AOSP GPL/LGPL, più
  il manifest di repo fissato e le patch di Vetro: si pubblicano insieme
  alle immagini.
- I file del device e le configurazioni sono codice di Vetro (PolyForm
  Noncommercial 1.0.0, ADR 0004); per Soong `legacy_notice`, perché la
  licenza non ha un tipo SPDX nel build system.

## Alternative scartate
- **Immagini Cuttlefish così come sono**, con un host PCIe ECAM in Vetro:
  resta possibile (ADR futuro), ma richiede u-boot o un launcher, e AVB.
- **Immagine ranchu dell'emulatore ricostruita** con composizione software:
  goldfish in ogni HAL, nessun vantaggio su Cuttlefish.
- **Kernel 6.18 di M3**: mancano binder, eBPF, dm-verity, e la ABI dei
  moduli GKI.
- **Spoofing della firma generale** (qualsiasi app con il permesso):
  inutilmente largo; bastano due pacchetti con un certificato noto.

## Conseguenze
- L'immagine si ricostruisce con `tools/aosp/build.sh start|wait`,
  `tools/aosp/fetch.sh`, e si avvia con `tools/aosp/qemu.sh` (oracolo) e
  `tools/aosp/vetro.sh`, con gli stessi dispositivi negli stessi slot.
- Aggiornare AOSP = cambiare tag, rifare `repo sync`, controllare che la
  patch di `frameworks/base` si applichi (`prepare.sh` si ferma se no).
- Aggiornare microG = nuovo `microg.lock` e allowlist rigenerata.
- Da fare: SELinux enforcing, virtio-rng deterministico in Vetro (il modulo
  è già nella prima fase), adb su canale virtio per il browser, disco
  via HTTP Range (M6).
