//! Accelerated guest graphics in the page (ADR 0037): the gfxstream decoder's
//! op batches go to the JS WebGL2 executor (`web/app/gl.mjs`) through the
//! import `vetro_host.gl_execute`; one call per batch, with pointers into
//! this module's memory (words, blob, and the buffer the read ops fill).

use vetro_machine::vetro_gfxstream::{Gfxstream, GlExecutor};
use vetro_platform::virtio::VirtioGpu;

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "vetro_host")]
    unsafe extern "C" {
        /// Runs one batch of ops: `words` u32 words, `blob` bytes, and the
        /// `out` bytes the read ops fill, in order.
        pub fn gl_execute(
            words: *const u32,
            nwords: usize,
            blob: *const u8,
            nblob: usize,
            out: *mut u8,
            nout: usize,
        );
    }
}

/// Executor in JS.
#[derive(Default)]
pub struct JsExecutor;

impl GlExecutor for JsExecutor {
    #[cfg(target_arch = "wasm32")]
    fn execute(&mut self, words: &[u32], blob: &[u8], out: &mut [u8]) {
        // SAFETY: `vetro_host` import; it reads and writes the three buffers
        // only during the call.
        unsafe {
            host::gl_execute(
                words.as_ptr(),
                words.len(),
                blob.as_ptr(),
                blob.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(&mut self, _words: &[u32], _blob: &[u8], _out: &mut [u8]) {}
}

/// Gives the machine's gfxstream renderer the JS executor. False if the
/// GPU has no 3D renderer.
pub fn enable(gpu: &mut VirtioGpu) -> bool {
    match gpu.renderer_as_mut::<Gfxstream>() {
        Some(r) => {
            r.gl.exec = Box::new(JsExecutor);
            true
        }
        None => false,
    }
}

/// The pixels of the ColorBuffer shown on `scanout`, as RGBA rows from the
/// top of the screen (the texture's row 0 is the bottom, like upstream's
/// post). A host read for the page (screenshots, home-screen detection):
/// never visible to the guest.
pub fn scanout_rgba(gpu: &mut VirtioGpu, scanout: u32) -> Option<(u32, u32, Vec<u8>)> {
    let (res, rect) = gpu.scanout_3d(scanout)?;
    let r = gpu.renderer_as_mut::<Gfxstream>()?;
    let tex = r.gl.cbs.get(&res)?.tex;
    let (w, h, mut px) = r.read_color_buffer(res)?;
    if tex.bpp != 4 {
        return None;
    }
    if tex.swizzle {
        // Guest byte order (BGRA) back to RGBA.
        vetro_machine::vetro_gfxstream::formats::swap_rb(&mut px);
    }
    let (rw, rh) = (rect.width.min(w), rect.height.min(h));
    let mut out = vec![0u8; (rw * rh * 4) as usize];
    for y in 0..rh {
        // Scanout row y (from the top) = texture row h - 1 - (rect.y + y).
        let src_row = h - 1 - (rect.y + y);
        let s = ((src_row * w + rect.x) * 4) as usize;
        let d = (y * rw * 4) as usize;
        out[d..d + (rw * 4) as usize].copy_from_slice(&px[s..s + (rw * 4) as usize]);
    }
    Some((rw, rh, out))
}
