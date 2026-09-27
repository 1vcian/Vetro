# ADR 0036 — Accelerated guest graphics: gfxstream GLES over virtio-gpu 3D, executed by WebGL2

- Status: accepted (M5, 2026-09-27), first slice behind a flag (see
  "Slices"). Builds on ADR 0022/0030/0032 (the `vetro_arm64` image), 0028
  (AOSP in the browser), 0015 (snapshots), 0019 (replay inputs), 0035
  (bootconfig overrides from the host). Details: `docs/specs/gfxstream.md`,
  `docs/specs/platform.md` (virtio-gpu 3D).
- Number: on rebase onto main, renumber if 0036 is already taken.

## Context
The image renders with SwiftShader (Vulkan "pastel") under ANGLE: every
SurfaceFlinger composition and every HWUI frame is rasterised by the emulated
Cortex-A53, then copied to the virtio-gpu 2D scanout. Measured on the image
`…-64fcd35`, it is the largest single cost of the UI after boot (ADR 0032:
SurfaceFlinger, the composer and a RenderThread taking most of the one CPU
while only a progress bar animates). The goal: guest GL (and later Vulkan)
commands executed by the host browser's GPU through WebGL2 or WebGPU, with
SwiftShader kept as the fallback.

What the AOSP 15 tree and our build already contain (checked on the build
VM, `android-15.0.0_r36`, `out/target/product/vetro_arm64`):
- **gfxstream guest GLES**: `/vendor/lib64/egl/libEGL_emulation.so`,
  `libGLESv1_CM_emulation.so`, `libGLESv2_emulation.so`,
  `libOpenglSystemCommon.so`, `lib_renderControl_enc.so` (Cuttlefish's
  `shared/graphics/device_vendor.mk` installs them unconditionally).
- **gfxstream guest Vulkan**: `vulkan.ranchu` inside `com.google.cf.vulkan`,
  next to SwiftShader's `vulkan.pastel`.
- **No Mesa GL**: `shared/virgl/device_vendor.mk` asks for `libGLES_mesa`,
  but no module of that name exists in this tree's `external/mesa3d` (it
  only builds the gfxstream/venus guest parts): nothing virgl in the image.
- **The choice is made at boot**: `init_graphics.vendor.rc` copies
  `ro.boot.hardware.{egl,vulkan,gralloc,hwcomposer,hwcomposer.mode,
  gltransport}` and `ro.boot.opengles.version` into the `ro.hardware.*`
  properties the loaders read, and Vetro's bootloader can replace the
  vendor_boot lines (ADR 0028). Cuttlefish's gem5 target runs gfxstream with
  `gltransport=virtio-gpu-pipe`, the transport for a VMM without shared host
  memory — our situation.
- The gfxstream "virtio-gpu-pipe" transport
  (`hardware/google/gfxstream/guest/OpenglSystemCommon/VirtioGpuPipeStream.cpp`)
  needs only classic virtio-gpu 3D: a 1 MiB `PIPE_BUFFER` resource per GL
  thread with guest backing, written with TRANSFER_TO_HOST_3D and read with
  TRANSFER_FROM_HOST_3D; no blob resources, no host-visible memory region, no
  context types. minigbm uses its virgl backend when `VIRTGPU_PARAM_3D_FEATURES`
  is set and, with no capset, accepts every format; the gralloc buffer's
  virtio-gpu resource id *is* the gfxstream ColorBuffer handle
  (`GrallocMinigbm::getHostHandle`). With `hwcomposer.mode=client` the ranchu
  HWC (`ClientFrameComposer`) only commits SurfaceFlinger's client target to
  the DRM plane: SET_SCANOUT of a 3D resource plus RESOURCE_FLUSH.

## Options
The owner's guidance for this choice: prefer standard protocols and
existing, maintained upstream components over bespoke ones, use Chrome's
native capabilities (WebGL2/WebGPU from an `OffscreenCanvas` in the Worker,
WASM SIMD and threads where they help), score maintenance burden explicitly,
and pick the least custom option that works in Chrome.

| | A. virgl (Gallium) | B. Venus (Vulkan) | C. gfxstream: upstream host renderer compiled to WASM | D. gfxstream protocol, host decoder generated from upstream specs | E. custom GLES forwarding |
|---|---|---|---|---|---|
| Guest side in this image | none: Mesa GL must be ported into the build | venus not built; needs blobs + host-visible memory | **present** (upstream AOSP), selected by bootconfig | **present** (upstream AOSP), selected by bootconfig | new EGL/GLES/gralloc libraries to write |
| Protocol | standard (virgl) | standard (Venus) | standard (gfxstream/emugen) | **standard (gfxstream/emugen)**, decoder tables generated from the upstream `.in/.attrib/.types` | ours |
| Host code | virglrenderer's vrend (~40k lines of C) compiled to WASM: needs desktop GL 3.3 or GLES 3.1+ (texture buffers, geometry shaders…), one GL context per guest context with shared objects | Vulkan→WebGPU translation: no upstream component exists (SPIR-V→WGSL, descriptor model, persistent mapping WebGPU forbids) | gfxstream's FrameBuffer/RenderThread/GLES translator: one host thread per guest thread, many host contexts **sharing** textures (EGL share groups), EGL pbuffers, dlopen'd GL; WebGL has no shared contexts and a context lives on one thread, so it needs an invasive fork plus the Emscripten C++ toolchain next to our Rust wasm32 build | generic packet parser driven by generated tables (verified against the upstream generated decoders for all 509 opcodes), renderControl/EGL semantics as upstream's `RenderControl.cpp`, a thin GLES 3.0→WebGL2 executor in JS; ANGLE inside Chrome does the real translation | everything, both sides |
| Chrome fit | WebGL2 below vrend's floor | WebGPU is not Vulkan | blocked by context sharing and threading | **direct**: guest reports GLES 3.0 = WebGL2; all guest contexts mapped onto one WebGL2 context of an `OffscreenCanvas` in the Worker | direct |
| Virtio needs | 3D + capsets | 3D + blobs + context init + shm region | 3D (pipe) | **3D only** (pipe transport) | 3D only |
| **Maintenance burden** (per AOSP/upstream update) | very high: Mesa Android build + vrend port on WebGL | very high: our own Vulkan→WebGPU layer | high: rebase an invasive fork of the host renderer, two toolchains | **medium-low**: guest untouched; protocol changes = rerun `tools/gfxstream/gen-tables.py` (CI checks the tables are current); our semantic layer follows `RenderControl.cpp`/`GLESv2Decoder.cpp` | very high: guest drivers to maintain forever |
| Effort to first frame | very high | very high | high | **medium** | high |

Compiling only upstream's *generated* decoders (`gles2_dec.cpp`,
`renderControl_dec.cpp`) to WASM was also weighed under D: they only
unpack parameters, which the table-driven parser reproduces exactly (checked
opcode by opcode), while the parts that carry meaning (`GLESv2Decoder.cpp`'s
custom calls, ColorBuffers, EGL objects) are entangled with the upstream
translator and EGL; the C++ toolchain would buy no semantics.

Determinism, snapshots and security weigh the same on every option once the
host side runs inside Vetro (below); they do not change the ranking. **D is
the least custom option that works in Chrome**: standard protocol, upstream
guest drivers, upstream-derived tables, and Chrome's own ANGLE behind WebGL2.

## Decision
1. **gfxstream GLES over virtio-gpu 3D with the pipe transport**, host side
   written by us, executed by **WebGL2** in the Worker. The guest reports
   **GLES 3.0** (`opengles.version=196608`, host extension
   `ANDROID_EMU_gles_max_version_3_0`): exactly WebGL2's level.
2. **Selected at boot by the host**, no image rebuild needed for the first
   slice: when the page has WebGL2 with the required limits and the flag is on
   (`?gpu=webgl`), the machine offers `VIRTIO_GPU_F_VIRGL` and the bootloader
   appends `androidboot.hardware.egl=emulation`,
   `androidboot.hardware.gltransport=virtio-gpu-pipe`,
   `androidboot.hardware.hwcomposer.mode=client`,
   `androidboot.opengles.version=196608` (replacing the vendor lines).
   Otherwise nothing changes: SwiftShader, 2D virtio-gpu, same snapshot key.
   `ro.hardware.vulkan` stays `pastel` (apps that use Vulkan keep SwiftShader;
   HWUI is `skiagl` because `ro.hwui.use_vulkan` is empty).
3. **virtio-gpu 3D in `vetro-platform`** (spec: `docs/specs/platform.md`):
   feature `VIRGL`, `num_capsets = 0`, CTX_CREATE/DESTROY/ATTACH/DETACH,
   RESOURCE_CREATE_3D, TRANSFER_TO/FROM_HOST_3D, SUBMIT_3D, 3D resources as
   scanouts; the protocol lives behind a `Renderer3d` trait, so the device
   stays independent of gfxstream.
4. **The decoder is Rust (`vetro-gfxstream`)**, as close to upstream as
   possible: the wire tables are generated (never hand-written), the
   renderControl and EGL behaviour follows upstream's host
   (`RenderControl.cpp`, `FrameBuffer.cpp`, `GLESv2Decoder.cpp`), deviations
   are listed in `docs/specs/gfxstream.md`. It handles the pipe services
   (`GLProcessPipe`, `opengles`), renderControl and GLES2/3 decoding from
   tables generated from gfxstream's emugen specs (Apache 2.0), EGL objects
   (contexts, surfaces, ColorBuffers = resource ids), guest→host object
   names, shadow state. It emits a compact **op stream** (u32 words) to a
   `GlExecutor`: in the browser a JS module that runs the ops on a WebGL2
   context of an `OffscreenCanvas` in the Worker, in batches (one crossing per
   flush, not per call); natively a recorder (for tests and for replaying a
   guest session in headless Chrome) and a null executor.
5. **Presentation without readback**: a 3D scanout is drawn by the executor
   from the ColorBuffer's texture into its canvas and handed to the page as an
   `ImageBitmap` (transferable). The 2D path and its presenters stay as they
   are.
6. **Chrome's native paths**: WebGL2 on an `OffscreenCanvas` owned by the
   emulator Worker (the executor runs where the guest's commands are decoded,
   so readbacks are synchronous without cross-thread hops; Chrome's GPU
   process already runs ANGLE in parallel with the Worker); pixel format
   conversions (BGRA↔RGBA, row flips) in Rust with WASM SIMD where the build
   enables it; WebGPU is kept for the Vulkan slice.

### Determinism (ADR 0019)
GPU output must not reach guest-visible state except where recorded:
- **Everything the guest can query is answered by Rust from a fixed profile**,
  never from the host GPU: GL strings, extensions, limits
  (`glGetIntegerv`…), EGL configs, compile/link status (always success;
  host errors go to the host log), uniform and attribute locations (virtual,
  assigned by the decoder in query order and mapped to WebGL locations by the
  executor), `glGetError` (guest-side tracking only, `noHostError`), sync
  objects (signalled on creation: execution is synchronous).
- **Readbacks are the only exceptions**, listed exhaustively: `glReadPixels`,
  `rcReadColorBuffer` / `rcReadColorBufferYUV` / `…DMA`,
  TRANSFER_FROM_HOST_3D of a ColorBuffer resource, `glMapBufferRangeAEMU`
  with read access, query objects results. Their bytes are host inputs: they
  go into the replay log (one event per readback) and are replayed from it;
  the same host with the same browser gives the same bytes, different GPUs
  may not.
- The profile is part of the machine configuration (snapshot key): a
  snapshot never resumes under different reported limits.

### Snapshots (ADR 0015)
The decoder's shadow state is part of the device state: EGL objects,
ColorBuffers, per-share-group objects with their creation parameters,
shader sources, program attribute bindings and link state, uniform values,
the GL state vector of every context (bindings, enables, blend, depth,
stencil, viewport, pixel store, vertex arrays). Pixel contents that only the
GPU has (textures, renderbuffers, ColorBuffers, buffer objects written by the
GPU) are **read back by the executor at save time** and stored compressed;
restore recreates everything on a fresh WebGL2 context and uploads the data.
A snapshot taken with the GPU path restores only with the GPU path (the key
includes the mode); the fallback path is unchanged.

### Security
- The executor receives only ops produced by the decoder, never guest bytes
  to interpret; every size in them was checked against the guest buffer it
  came from; the op stream lives in wasm memory, so nothing outside the
  module's linear memory can be named.
- WebGL2 is designed for untrusted content: the browser validates every
  call and translates every shader (ANGLE); this is the main security
  advantage over native GL forwarding in QEMU/crosvm.
- Host memory is bounded: ColorBuffers and GL objects count against
  `max_hostmem`, pipes have bounded buffers, object tables have limits; a
  guest exceeding them gets GL errors, not host failures.

## Slices
1. **This ADR's slice**: virtio-gpu 3D, the pipe, renderControl and the
   GLES 3.0 subset SurfaceFlinger (RenderEngine, Skia GL), HWUI and the
   launcher use; WebGL2 executor in the Worker; client composition; boot
   flag; replay and snapshot of the decoder state; the SwiftShader path
   untouched.
2. HostFrameComposer (`rcCompose`: device composition on the host GPU,
   no SurfaceFlinger GLES pass for simple frames); native fence sync
   (`ANDROID_EMU_virtio_gpu_native_sync`, fences signalled synchronously).
3. **Vulkan → WebGPU** through `vulkan.ranchu` (gfxstream Vulkan): needs blob
   resources and a host-visible memory region on virtio-mmio (SHM registers),
   the ASG ring, and a Vulkan→WebGPU translation; a separate ADR.

## Rejected alternatives
- **virgl**: no guest driver in the tree (Mesa GL would have to be ported
  into the build) and a TGSI→GLSL ES translator plus Gallium state tracking
  on the host; the WebGL2 feature floor is below what vrend expects.
- **Venus first**: the richest API, but it needs blob resources and shared
  host memory, and Vulkan does not map onto WebGPU without a translation layer
  of its own; the GLES path gives the UI its GPU sooner. Kept for slice 3
  through gfxstream Vulkan instead, whose guest driver is already in the image.
- **Custom GLES forwarding**: gfxstream already is that, with its guest side
  maintained upstream and installed in the image.
- **gfxstream's own host renderer compiled to wasm (C)**: WebGL has no shared contexts and binds a context to one thread, while the renderer shares textures across many host contexts and threads; it also targets desktop GL
  or EGL/GLES 3.1 through its translator, keeps threads per guest thread and
  process-wide globals; its snapshot and determinism rules are not ours.
- **Decoder in JS**: the device, its snapshot and the replay log live in
  Rust; a JS decoder would need a second state serializer and could not be
  unit-tested natively with the machine.
- **ASG transport first**: needs blob resources and host-visible memory;
  the pipe transport works with plain 3D resources (gem5 uses it).

## Consequences
- A new crate `vetro-gfxstream`, a new spec `docs/specs/gfxstream.md`, a new
  ABI surface in `vetro-wasm` (executor imports), `web/app/gl.mjs` (executor).
- The same image boots either path; the prebuilt snapshot (ADR 0031) stays
  valid for the SwiftShader path; the GPU path needs its own.
- Image changes for the next rebuild are optional for slice 1; the
  graphics defaults of a GPU-first image and the low-RAM profile are prepared
  as separate, switchable parts (`docs/specs/guest-image.md`).
