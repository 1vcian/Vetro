//! virgl formats of guest buffers (minigbm) and how their ColorBuffers live
//! in WebGL2 textures, following upstream's `virgl_format_to_gl`
//! (`VirtioGpuFormatUtils.h`). WebGL2 has no BGRA textures: BGRA buffers
//! are RGBA textures and their bytes are swizzled on the way in and out.

pub const VIRGL_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const VIRGL_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub const VIRGL_FORMAT_B5G6R5_UNORM: u32 = 7;
pub const VIRGL_FORMAT_R10G10B10A2_UNORM: u32 = 8;
pub const VIRGL_FORMAT_Z16_UNORM: u32 = 16;
pub const VIRGL_FORMAT_Z32_FLOAT: u32 = 18;
pub const VIRGL_FORMAT_Z24_UNORM_S8_UINT: u32 = 19;
pub const VIRGL_FORMAT_Z24X8_UNORM: u32 = 21;
pub const VIRGL_FORMAT_R16_UNORM: u32 = 48;
pub const VIRGL_FORMAT_R8_UNORM: u32 = 64;
pub const VIRGL_FORMAT_R8G8_UNORM: u32 = 65;
pub const VIRGL_FORMAT_R8G8B8_UNORM: u32 = 66;
pub const VIRGL_FORMAT_R8G8B8A8_UNORM: u32 = 67;
pub const VIRGL_FORMAT_R16G16B16A16_FLOAT: u32 = 94;
pub const VIRGL_FORMAT_Z32_FLOAT_S8X24_UINT: u32 = 126;
pub const VIRGL_FORMAT_R8G8B8X8_UNORM: u32 = 134;

pub const PIPE_BUFFER: u32 = 0;
pub const VIRGL_BIND_RENDER_TARGET: u32 = 1 << 1;
pub const VIRGL_BIND_SAMPLER_VIEW: u32 = 1 << 3;
pub const VIRGL_BIND_CURSOR: u32 = 1 << 16;
pub const VIRGL_BIND_SCANOUT: u32 = 1 << 18;
pub const VIRGL_BIND_LINEAR: u32 = 1 << 22;

// GL enums.
pub const GL_RGBA: u32 = 0x1908;
pub const GL_RGB: u32 = 0x1907;
pub const GL_RED: u32 = 0x1903;
pub const GL_RG: u32 = 0x8227;
pub const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub const GL_UNSIGNED_SHORT_5_6_5: u32 = 0x8363;
pub const GL_UNSIGNED_INT_2_10_10_10_REV: u32 = 0x8368;
pub const GL_HALF_FLOAT: u32 = 0x140B;
pub const GL_RGBA8: u32 = 0x8058;
pub const GL_RGB8: u32 = 0x8051;
pub const GL_RGB565: u32 = 0x8D62;
pub const GL_R8: u32 = 0x8229;
pub const GL_RG8: u32 = 0x822B;
pub const GL_RGB10_A2: u32 = 0x8059;
pub const GL_RGBA16F: u32 = 0x881A;
pub const GL_DEPTH_COMPONENT: u32 = 0x1902;
pub const GL_DEPTH_STENCIL: u32 = 0x84F9;
pub const GL_DEPTH_COMPONENT16: u32 = 0x81A5;
pub const GL_DEPTH_COMPONENT24: u32 = 0x81A6;
pub const GL_DEPTH_COMPONENT32F: u32 = 0x8CAC;
pub const GL_DEPTH24_STENCIL8: u32 = 0x88F0;
pub const GL_DEPTH32F_STENCIL8: u32 = 0x8CAD;
pub const GL_UNSIGNED_SHORT: u32 = 0x1403;
pub const GL_UNSIGNED_INT: u32 = 0x1405;
pub const GL_UNSIGNED_INT_24_8: u32 = 0x84FA;
pub const GL_FLOAT: u32 = 0x1406;
pub const GL_FLOAT_32_UNSIGNED_INT_24_8_REV: u32 = 0x8DAD;

/// How a ColorBuffer is stored: texture internal format, and the
/// format/type of its bytes in guest memory after [`Tex::swizzle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tex {
    pub internal: u32,
    pub format: u32,
    pub ty: u32,
    /// Bytes per pixel in guest memory.
    pub bpp: u32,
    /// Guest bytes are B,G,R,A: swap R and B to and from the RGBA texture.
    pub swizzle: bool,
    /// Uploads and readbacks are supported (depth formats and YUV are not).
    pub transferable: bool,
}

/// The texture of a ColorBuffer of virgl format `f`.
pub fn tex_of_virgl(f: u32) -> Tex {
    let t = |internal, format, ty, bpp| Tex { internal, format, ty, bpp, swizzle: false, transferable: true };
    match f {
        VIRGL_FORMAT_B8G8R8A8_UNORM | VIRGL_FORMAT_B8G8R8X8_UNORM => {
            Tex { swizzle: true, ..t(GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE, 4) }
        }
        VIRGL_FORMAT_R8G8B8A8_UNORM | VIRGL_FORMAT_R8G8B8X8_UNORM => {
            t(GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE, 4)
        }
        VIRGL_FORMAT_B5G6R5_UNORM => t(GL_RGB565, GL_RGB, GL_UNSIGNED_SHORT_5_6_5, 2),
        VIRGL_FORMAT_R8_UNORM => t(GL_R8, GL_RED, GL_UNSIGNED_BYTE, 1),
        VIRGL_FORMAT_R8G8_UNORM => t(GL_RG8, GL_RG, GL_UNSIGNED_BYTE, 2),
        VIRGL_FORMAT_R8G8B8_UNORM => t(GL_RGB8, GL_RGB, GL_UNSIGNED_BYTE, 3),
        VIRGL_FORMAT_R10G10B10A2_UNORM => t(GL_RGB10_A2, GL_RGBA, GL_UNSIGNED_INT_2_10_10_10_REV, 4),
        VIRGL_FORMAT_R16G16B16A16_FLOAT => t(GL_RGBA16F, GL_RGBA, GL_HALF_FLOAT, 8),
        // No 16-bit normalized textures in WebGL2 core: stored as R8, not
        // transferable (camera formats, outside this slice).
        VIRGL_FORMAT_R16_UNORM => Tex { transferable: false, ..t(GL_R8, GL_RED, GL_UNSIGNED_BYTE, 2) },
        VIRGL_FORMAT_Z16_UNORM => {
            Tex { transferable: false, ..t(GL_DEPTH_COMPONENT16, GL_DEPTH_COMPONENT, GL_UNSIGNED_SHORT, 2) }
        }
        VIRGL_FORMAT_Z24X8_UNORM => {
            Tex { transferable: false, ..t(GL_DEPTH_COMPONENT24, GL_DEPTH_COMPONENT, GL_UNSIGNED_INT, 4) }
        }
        VIRGL_FORMAT_Z24_UNORM_S8_UINT => {
            Tex { transferable: false, ..t(GL_DEPTH24_STENCIL8, GL_DEPTH_STENCIL, GL_UNSIGNED_INT_24_8, 4) }
        }
        VIRGL_FORMAT_Z32_FLOAT => {
            Tex { transferable: false, ..t(GL_DEPTH_COMPONENT32F, GL_DEPTH_COMPONENT, GL_FLOAT, 4) }
        }
        VIRGL_FORMAT_Z32_FLOAT_S8X24_UINT => Tex {
            transferable: false,
            ..t(GL_DEPTH32F_STENCIL8, GL_DEPTH_STENCIL, GL_FLOAT_32_UNSIGNED_INT_24_8_REV, 8)
        },
        // YUV (NV12, NV21, YV12, P010) and anything else: RGBA8888 like
        // upstream, content not converted in this slice.
        _ => Tex { transferable: false, ..t(GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE, 4) },
    }
}

/// The texture of a ColorBuffer created with a GL internal format
/// (rcCreateColorBuffer).
pub fn tex_of_gl(internal: u32) -> Tex {
    match internal {
        GL_RGB | GL_RGB8 => tex_of_virgl(VIRGL_FORMAT_R8G8B8_UNORM),
        GL_RGB565 => tex_of_virgl(VIRGL_FORMAT_B5G6R5_UNORM),
        GL_RGBA16F => tex_of_virgl(VIRGL_FORMAT_R16G16B16A16_FLOAT),
        GL_RGB10_A2 => tex_of_virgl(VIRGL_FORMAT_R10G10B10A2_UNORM),
        0x80E1 => tex_of_virgl(VIRGL_FORMAT_B8G8R8A8_UNORM), // GL_BGRA_EXT
        _ => tex_of_virgl(VIRGL_FORMAT_R8G8B8A8_UNORM),
    }
}

/// Swaps bytes 0 and 2 of every 4-byte pixel (BGRA ↔ RGBA), in place.
pub fn swap_rb(px: &mut [u8]) {
    for p in px.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
}

/// Bytes of a `w`x`h` image of GL `format`/`type` with the default pack or
/// unpack alignment of 4 not applied (tightly packed rows).
pub fn bytes_per_pixel(format: u32, ty: u32) -> u32 {
    let comps = match format {
        GL_RED | 0x8D94 | GL_DEPTH_COMPONENT | 0x1906 | 0x1909 => 1, // RED_INTEGER, ALPHA, LUMINANCE
        GL_RG | 0x8228 | GL_DEPTH_STENCIL | 0x190A => 2,             // RG_INTEGER, LUMINANCE_ALPHA
        GL_RGB | 0x8D98 => 3,                                        // RGB_INTEGER
        _ => 4,
    };
    match ty {
        GL_UNSIGNED_SHORT_5_6_5 | 0x8033 | 0x8034 => 2, // 4_4_4_4, 5_5_5_1
        GL_UNSIGNED_INT_2_10_10_10_REV | GL_UNSIGNED_INT_24_8 | 0x8C3B | 0x8C3E => 4,
        GL_FLOAT_32_UNSIGNED_INT_24_8_REV => 8,
        GL_UNSIGNED_BYTE | 0x1400 => comps,
        GL_UNSIGNED_SHORT | 0x1402 | GL_HALF_FLOAT => 2 * comps,
        _ => 4 * comps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_upstream() {
        let bgra = tex_of_virgl(VIRGL_FORMAT_B8G8R8A8_UNORM);
        assert!(bgra.swizzle && bgra.internal == GL_RGBA8 && bgra.bpp == 4);
        assert!(!tex_of_virgl(VIRGL_FORMAT_R8G8B8A8_UNORM).swizzle);
        assert_eq!(tex_of_virgl(VIRGL_FORMAT_B5G6R5_UNORM).ty, GL_UNSIGNED_SHORT_5_6_5);
        assert!(!tex_of_virgl(VIRGL_FORMAT_Z24_UNORM_S8_UINT).transferable);
        let mut px = [1, 2, 3, 4, 5, 6, 7, 8];
        swap_rb(&mut px);
        assert_eq!(px, [3, 2, 1, 4, 7, 6, 5, 8]);
        assert_eq!(bytes_per_pixel(GL_RGBA, GL_UNSIGNED_BYTE), 4);
        assert_eq!(bytes_per_pixel(GL_RGB, GL_UNSIGNED_SHORT_5_6_5), 2);
        assert_eq!(bytes_per_pixel(GL_RGBA, GL_HALF_FLOAT), 8);
    }
}
