# ADR 0047 — The browser's GPU draws Android's UI: gfxstream on WebGL2 by default

- Status: accepted (M5, 2026-10-02). Builds on ADR 0037 (gfxstream GLES over
  virtio-gpu 3D, WebGL2 executor), 0031 (prebuilt snapshot), 0043 (image
  f08b79e), 0044 (prefetch list, warmed prebuilt). Details:
  `docs/specs/gfxstream.md`; measurements: `docs/progress/M5.md`
  (2026-10-02, "gfxstream to the home screen").

## Context
ADR 0037's first slice reached SurfaceFlinger's boot animation natively; the
default path still rendered every frame with SwiftShader on the emulated
Cortex-A53, which ADR 0044's live-path measurements show is the main cost the
user sees (the guest runs at 0.3-0.5x the clock during each step, much of it
in SwiftShader's FP code).

Taking the accelerated path to the home screen found four bugs, all in our
host side (the guest image needed no change):
1. **Row order.** Window surfaces were drawn straight into the ColorBuffer
   with GL's bottom row first, while Android's buffers hold the top row
   first (gralloc memory, CPU uploads, SurfaceFlinger's sampling and its own
   FBO render target): the status bar was composed at the bottom of the
   screen.
2. **Preserved swaps.** The guest's EGL reports `EGL_BUFFER_PRESERVED` (the
   configs carry `EGL_SWAP_BEHAVIOR_PRESERVED_BIT`), so HWUI redraws only
   the damaged region; drawing into the rotating gralloc buffers lost
   everything else (a clock showing one digit, half-drawn screens).
3. **Storage textures in snapshots.** `glTexStorage2D` textures (Skia's glyph
   atlases and bitmaps) had no level records, so a restored snapshot had
   empty atlases: no text, no icons until each app redrew from scratch.
4. **Pbuffer copy on bind** (from the fix of 1-2): copying a surface into a
   ColorBuffer when it is bound as a texture must be limited to pbuffers'
   own ColorBuffers, or SurfaceFlinger's binding of a gralloc buffer the
   app's surface still points at overwrote its flushed rows (the wallpaper
   upside down).

## Decision
1. Window surfaces work like upstream's: their own preserved color texture,
   copied into the ColorBuffer at `rcFlushWindowColorBuffer` with a flipped
   `blitFramebuffer` (top row first); presenting draws row 0 at the top.
   Snapshots save the surfaces' colors and the levels of storage textures
   (gfxstream snapshot version 2).
2. **Snapshots on the accelerated path in the page**, keyed apart from the
   SwiftShader path's (the `GPU_3D` device bit and the boot parameters are in
   the key); a restored 3D scanout is presented at once. Its **prebuilt
   snapshot is made by the app itself in headless Chrome**
   (`tools/aosp/prebuilt-chrome.mjs`: cold boot, settle, warm-up,
   compaction, the snapshot read out of OPFS), because the GPU contents can
   only be read back through WebGL2; published next to the image like
   ADR 0031's, with its own prefetch list (ADR 0044).
3. **The page chooses by itself**: `DEFAULT_GPU` in `web/app/main.mjs`;
   `?gpu=webgl` / `?gpu=swiftshader` override it for tests. Without WebGL2
   in the Worker (or below the profile's limits) the Worker keeps
   SwiftShader and says why: no choice for the user
   (`tests/web/gl.mjs` checks both).
4. No new image version: f08b79e already contains gfxstream's guest GLES,
   and the boot parameters select it (ADR 0037).

## Measurements
`tools/aosp/live-path.mjs --webgl` on the build VM (light profile, one core,
image f08b79e, each path's warmed prebuilt; two runs each; WebGL2 is
Chrome's own SwiftShader on the host, the VM has no GPU), gfxstream against
SwiftShader: app drawer drawn 2.5-3.0 s against 9.4-52.2 s after the swipe;
Settings' window 16.3-20.6 s against 140.6-162.6 s after the tap, with 3-5
instead of 34-37 s of guest time; a home tap's first frame 3.1-4.1 s against
4.0-8.3 s; first frame after the restore 6.3-6.8 s against 10.1-13.4 s;
install and first screen of a catalog app the same (dominated by
package manager and dex work, not drawing). Full table:
`docs/progress/M5.md`. Clearly better and stable over three runs after the
fixes: published as the default (prebuilt `1779b7baf2263ed9b05d78ae538c0480`
and its prefetch list on R2).

## Consequences
- The prebuilt snapshot of the accelerated path is larger (GPU contents,
  surfaces): ~700 MiB against 483 MiB for the SwiftShader one.
- The cores stay in turns on the accelerated path (ADR 0042: WebGL lives on
  the machine's Worker); the live site is not cross-origin isolated, so it
  runs one core anyway.
- Guest Vulkan (`ro.hardware.vulkan=pastel`) is still SwiftShader; HWUI and
  SurfaceFlinger use GLES.
- Snapshots of the accelerated path are saved in the foreground, as in the
  measurements above: ADR 0046's deferred save copies the sections before
  the RAM uncompressed, and here they hold the GPU contents (hundreds of MiB
  more in wasm32 memory). The two were merged without having run together,
  so `saveSnapshotBackground` declines while the WebGL2 executor is in use;
  measuring them together is open work.
