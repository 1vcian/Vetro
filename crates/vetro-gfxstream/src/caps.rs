//! The fixed profile the guest sees (ADR 0037, determinism): GL and EGL
//! strings, limits and EGL configs never come from the host GPU. The values
//! are GLES 3.0's, within what every WebGL2 implementation offers; the page
//! checks the host meets them before choosing the GPU path.

/// Host extensions (renderControl, `ANDROID_EMU_*`) and GL extensions, in the
/// single string upstream's `rcGetGLString(GL_EXTENSIONS)` returns.
pub const GL_EXTENSIONS: &str = "GL_OES_EGL_image GL_OES_EGL_image_external GL_OES_EGL_image_external_essl3 \
GL_OES_vertex_array_object GL_OES_element_index_uint GL_OES_texture_npot GL_OES_rgb8_rgba8 \
GL_OES_depth24 GL_OES_depth_texture GL_OES_packed_depth_stencil GL_EXT_texture_rg \
ANDROID_EMU_gles_max_version_3_0 ANDROID_EMU_async_frame_commands ANDROID_EMU_async_unmap_buffer ";

pub const GL_VENDOR: &str = "Vetro";
pub const GL_RENDERER: &str = "Vetro WebGL2 (gfxstream)";
pub const GL_VERSION: &str = "OpenGL ES 3.0 Vetro";
pub const GL_SHADING_LANGUAGE_VERSION: &str = "OpenGL ES GLSL ES 3.00";

pub const EGL_VENDOR: &str = "Vetro";
pub const EGL_VERSION: &str = "1.4 Vetro";
pub const EGL_CLIENT_APIS: &str = "OpenGL_ES";
pub const EGL_EXTENSIONS: &str =
    "EGL_KHR_image_base EGL_KHR_gl_texture_2D_image EGL_KHR_create_context EGL_KHR_surfaceless_context ";

/// `glGetIntegerv` limits: (pname, values).
pub const LIMITS: &[(u32, &[i32])] = &[
    (0x0D33, &[4096]),        // MAX_TEXTURE_SIZE
    (0x851C, &[4096]),        // MAX_CUBE_MAP_TEXTURE_SIZE
    (0x84E8, &[4096]),        // MAX_RENDERBUFFER_SIZE
    (0x8073, &[256]),         // MAX_3D_TEXTURE_SIZE
    (0x88FF, &[256]),         // MAX_ARRAY_TEXTURE_LAYERS
    (0x0D3A, &[4096, 4096]),  // MAX_VIEWPORT_DIMS
    (0x8869, &[16]),          // MAX_VERTEX_ATTRIBS
    (0x8872, &[16]),          // MAX_TEXTURE_IMAGE_UNITS
    (0x8B4C, &[16]),          // MAX_VERTEX_TEXTURE_IMAGE_UNITS
    (0x8B4D, &[32]),          // MAX_COMBINED_TEXTURE_IMAGE_UNITS
    (0x8DFB, &[256]),         // MAX_VERTEX_UNIFORM_VECTORS
    (0x8DFD, &[224]),         // MAX_FRAGMENT_UNIFORM_VECTORS
    (0x8DFC, &[15]),          // MAX_VARYING_VECTORS
    (0x8B4A, &[1024]),        // MAX_VERTEX_UNIFORM_COMPONENTS
    (0x8B49, &[896]),         // MAX_FRAGMENT_UNIFORM_COMPONENTS
    (0x8B4B, &[60]),          // MAX_VARYING_COMPONENTS
    (0x9122, &[64]),          // MAX_VERTEX_OUTPUT_COMPONENTS
    (0x9125, &[60]),          // MAX_FRAGMENT_INPUT_COMPONENTS
    (0x8824, &[4]),           // MAX_DRAW_BUFFERS
    (0x8CDF, &[4]),           // MAX_COLOR_ATTACHMENTS
    (0x8D57, &[4]),           // MAX_SAMPLES
    (0x8A2F, &[24]),          // MAX_UNIFORM_BUFFER_BINDINGS
    (0x8A30, &[16384]),       // MAX_UNIFORM_BLOCK_SIZE
    (0x8A2B, &[12]),          // MAX_VERTEX_UNIFORM_BLOCKS
    (0x8A2D, &[12]),          // MAX_FRAGMENT_UNIFORM_BLOCKS
    (0x8A2E, &[24]),          // MAX_COMBINED_UNIFORM_BLOCKS
    (0x8A34, &[256]),         // UNIFORM_BUFFER_OFFSET_ALIGNMENT
    (0x8A31, &[199_680]),     // MAX_COMBINED_VERTEX_UNIFORM_COMPONENTS
    (0x8A33, &[198_656]),     // MAX_COMBINED_FRAGMENT_UNIFORM_COMPONENTS
    (0x8C8B, &[4]),           // MAX_TRANSFORM_FEEDBACK_SEPARATE_ATTRIBS
    (0x8C8A, &[64]),          // MAX_TRANSFORM_FEEDBACK_INTERLEAVED_COMPONENTS
    (0x8C80, &[4]),           // MAX_TRANSFORM_FEEDBACK_SEPARATE_COMPONENTS
    (0x80E8, &[150_000]),     // MAX_ELEMENTS_VERTICES
    (0x80E9, &[150_000]),     // MAX_ELEMENTS_INDICES
    (0x846E, &[1, 1]),        // ALIASED_LINE_WIDTH_RANGE
    (0x846D, &[1, 64]),       // ALIASED_POINT_SIZE_RANGE
    (0x0D50, &[4]),           // SUBPIXEL_BITS
    (0x8904, &[-8]),          // MIN_PROGRAM_TEXEL_OFFSET
    (0x8905, &[7]),           // MAX_PROGRAM_TEXEL_OFFSET
    (0x86A2, &[0]),           // NUM_COMPRESSED_TEXTURE_FORMATS
    (0x8DF9, &[0]),           // NUM_SHADER_BINARY_FORMATS
    (0x87FE, &[0]),           // NUM_PROGRAM_BINARY_FORMATS
    (0x8B9A, &[0x1401]),      // IMPLEMENTATION_COLOR_READ_TYPE = UNSIGNED_BYTE
    (0x8B9B, &[0x1908]),      // IMPLEMENTATION_COLOR_READ_FORMAT = RGBA
    (0x821B, &[3]),           // MAJOR_VERSION
    (0x821C, &[0]),           // MINOR_VERSION
    (0x8D6B, &[0x7FFF_FFFF]), // MAX_ELEMENT_INDEX (as GLint)
    (0x9111, &[0]),           // MAX_SERVER_WAIT_TIMEOUT
    (0x84FD, &[2]),           // MAX_TEXTURE_LOD_BIAS (as an integer)
];

pub fn limit(pname: u32) -> Option<&'static [i32]> {
    LIMITS.iter().find(|(p, _)| *p == pname).map(|(_, v)| *v)
}

/// GL extension names alone (for NUM_EXTENSIONS / glGetStringi).
pub fn gl_extension_list() -> Vec<&'static str> {
    GL_EXTENSIONS.split_whitespace().filter(|e| e.starts_with("GL_")).collect()
}

// EGL constants.
pub const EGL_NONE: u32 = 0x3038;
pub const EGL_DONT_CARE: i32 = -1;
const EGL_DEPTH_SIZE: u32 = 0x3025;
const EGL_STENCIL_SIZE: u32 = 0x3026;
const EGL_RENDERABLE_TYPE: u32 = 0x3040;
const EGL_SURFACE_TYPE: u32 = 0x3033;
const EGL_CONFIG_ID: u32 = 0x3028;
const EGL_BUFFER_SIZE: u32 = 0x3020;
const EGL_ALPHA_SIZE: u32 = 0x3021;
const EGL_BLUE_SIZE: u32 = 0x3022;
const EGL_GREEN_SIZE: u32 = 0x3023;
const EGL_RED_SIZE: u32 = 0x3024;
const EGL_CONFIG_CAVEAT: u32 = 0x3027;
const EGL_LEVEL: u32 = 0x3029;
const EGL_MAX_PBUFFER_HEIGHT: u32 = 0x302A;
const EGL_MAX_PBUFFER_PIXELS: u32 = 0x302B;
const EGL_MAX_PBUFFER_WIDTH: u32 = 0x302C;
const EGL_NATIVE_RENDERABLE: u32 = 0x302D;
const EGL_NATIVE_VISUAL_ID: u32 = 0x302E;
const EGL_NATIVE_VISUAL_TYPE: u32 = 0x302F;
const EGL_PRESERVED_RESOURCES: u32 = 0x3030;
const EGL_SAMPLES: u32 = 0x3031;
const EGL_SAMPLE_BUFFERS: u32 = 0x3032;
const EGL_TRANSPARENT_TYPE: u32 = 0x3034;
const EGL_TRANSPARENT_BLUE_VALUE: u32 = 0x3035;
const EGL_TRANSPARENT_GREEN_VALUE: u32 = 0x3036;
const EGL_TRANSPARENT_RED_VALUE: u32 = 0x3037;
const EGL_BIND_TO_TEXTURE_RGB: u32 = 0x3039;
const EGL_BIND_TO_TEXTURE_RGBA: u32 = 0x303A;
const EGL_MIN_SWAP_INTERVAL: u32 = 0x303B;
const EGL_MAX_SWAP_INTERVAL: u32 = 0x303C;
const EGL_LUMINANCE_SIZE: u32 = 0x303D;
const EGL_ALPHA_MASK_SIZE: u32 = 0x303E;
const EGL_COLOR_BUFFER_TYPE: u32 = 0x303F;
const EGL_RECORDABLE_ANDROID: u32 = 0x3142;
const EGL_CONFORMANT: u32 = 0x3042;

const EGL_RGB_BUFFER: i32 = 0x308E;
const EGL_WINDOW_BIT: i32 = 0x4;
const EGL_PBUFFER_BIT: i32 = 0x1;
const EGL_SWAP_BEHAVIOR_PRESERVED_BIT: i32 = 0x400;
const ES2_BIT: i32 = 0x4;
const ES3_BIT: i32 = 0x40;
const ES1_BIT: i32 = 0x1;

/// The attributes of a config, in upstream's order (`kConfigAttributes`:
/// depth, stencil, renderable type, surface type and config id first).
pub const CONFIG_ATTRIBS: [u32; 34] = [
    EGL_DEPTH_SIZE,
    EGL_STENCIL_SIZE,
    EGL_RENDERABLE_TYPE,
    EGL_SURFACE_TYPE,
    EGL_CONFIG_ID,
    EGL_BUFFER_SIZE,
    EGL_ALPHA_SIZE,
    EGL_BLUE_SIZE,
    EGL_GREEN_SIZE,
    EGL_RED_SIZE,
    EGL_CONFIG_CAVEAT,
    EGL_LEVEL,
    EGL_MAX_PBUFFER_HEIGHT,
    EGL_MAX_PBUFFER_PIXELS,
    EGL_MAX_PBUFFER_WIDTH,
    EGL_NATIVE_RENDERABLE,
    EGL_NATIVE_VISUAL_ID,
    EGL_NATIVE_VISUAL_TYPE,
    EGL_PRESERVED_RESOURCES,
    EGL_SAMPLES,
    EGL_SAMPLE_BUFFERS,
    EGL_TRANSPARENT_TYPE,
    EGL_TRANSPARENT_BLUE_VALUE,
    EGL_TRANSPARENT_GREEN_VALUE,
    EGL_TRANSPARENT_RED_VALUE,
    EGL_BIND_TO_TEXTURE_RGB,
    EGL_BIND_TO_TEXTURE_RGBA,
    EGL_MIN_SWAP_INTERVAL,
    EGL_MAX_SWAP_INTERVAL,
    EGL_LUMINANCE_SIZE,
    EGL_ALPHA_MASK_SIZE,
    EGL_COLOR_BUFFER_TYPE,
    EGL_RECORDABLE_ANDROID,
    EGL_CONFORMANT,
];

/// One EGL config: (r, g, b, a, depth, stencil).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub r: i32,
    pub g: i32,
    pub b: i32,
    pub a: i32,
    pub depth: i32,
    pub stencil: i32,
}

impl Config {
    pub const fn buffer_size(&self) -> i32 {
        self.r + self.g + self.b + self.a
    }
}

/// The configs: RGBA8888, RGBX8888 (alpha 0) and RGB565, each without and
/// with depth 24 / stencil 8. No multisampling (the guest's UI does not ask
/// for it; WebGL2 would resolve it anyway).
pub const CONFIGS: [Config; 6] = [
    Config { r: 8, g: 8, b: 8, a: 8, depth: 0, stencil: 0 },
    Config { r: 8, g: 8, b: 8, a: 8, depth: 24, stencil: 8 },
    Config { r: 8, g: 8, b: 8, a: 0, depth: 0, stencil: 0 },
    Config { r: 8, g: 8, b: 8, a: 0, depth: 24, stencil: 8 },
    Config { r: 5, g: 6, b: 5, a: 0, depth: 0, stencil: 0 },
    Config { r: 5, g: 6, b: 5, a: 0, depth: 24, stencil: 8 },
];

/// Value of `attr` for config index `i`.
pub fn config_attr(i: usize, attr: u32) -> i32 {
    let c = &CONFIGS[i];
    match attr {
        EGL_DEPTH_SIZE => c.depth,
        EGL_STENCIL_SIZE => c.stencil,
        EGL_RENDERABLE_TYPE => ES1_BIT | ES2_BIT | ES3_BIT,
        EGL_SURFACE_TYPE => EGL_WINDOW_BIT | EGL_PBUFFER_BIT | EGL_SWAP_BEHAVIOR_PRESERVED_BIT,
        EGL_CONFIG_ID => i as i32 + 1,
        EGL_BUFFER_SIZE => c.buffer_size(),
        EGL_ALPHA_SIZE => c.a,
        EGL_BLUE_SIZE => c.b,
        EGL_GREEN_SIZE => c.g,
        EGL_RED_SIZE => c.r,
        EGL_CONFIG_CAVEAT => EGL_NONE as i32,
        EGL_MAX_PBUFFER_HEIGHT | EGL_MAX_PBUFFER_WIDTH => 4096,
        EGL_MAX_PBUFFER_PIXELS => 4096 * 4096,
        EGL_NATIVE_VISUAL_ID => 0, // the guest's EGL sets it from the sizes
        EGL_SAMPLES | EGL_SAMPLE_BUFFERS => 0,
        EGL_TRANSPARENT_TYPE => EGL_NONE as i32,
        EGL_MIN_SWAP_INTERVAL => 0,
        EGL_MAX_SWAP_INTERVAL => 1,
        EGL_COLOR_BUFFER_TYPE => EGL_RGB_BUFFER,
        EGL_RECORDABLE_ANDROID => 1,
        EGL_CONFORMANT => ES1_BIT | ES2_BIT | ES3_BIT,
        _ => 0,
    }
}

/// `rcGetConfigs` layout: the attribute ids, then each config's values.
pub fn pack_configs() -> Vec<u32> {
    let mut v: Vec<u32> = CONFIG_ATTRIBS.to_vec();
    for i in 0..CONFIGS.len() {
        v.extend(CONFIG_ATTRIBS.iter().map(|&a| config_attr(i, a) as u32));
    }
    v
}

/// eglChooseConfig over [`CONFIGS`]: sizes are minimums, bit masks must be
/// included, EGL_CONFIG_ID matches exactly; unknown attributes (Android's
/// EGL_FRAMEBUFFER_TARGET_ANDROID…) are accepted. Sorted like EGL 1.4
/// §3.4.1: larger color first for requested components, then smaller
/// buffer, depth, stencil. Returns config indices.
pub fn choose_config(attribs: &[i32]) -> Vec<u32> {
    let mut want: Vec<(u32, i32)> = Vec::new();
    for pair in attribs.as_chunks::<2>().0 {
        if pair[0] as u32 == EGL_NONE {
            break;
        }
        want.push((pair[0] as u32, pair[1]));
    }
    let get = |a: u32| want.iter().rev().find(|(k, _)| *k == a).map(|(_, v)| *v);
    let mut found: Vec<u32> = (0..CONFIGS.len() as u32)
        .filter(|&i| {
            want.iter().all(|&(a, v)| {
                if v == EGL_DONT_CARE {
                    return true;
                }
                let have = config_attr(i as usize, a);
                match a {
                    EGL_DEPTH_SIZE | EGL_STENCIL_SIZE | EGL_BUFFER_SIZE | EGL_ALPHA_SIZE | EGL_BLUE_SIZE
                    | EGL_GREEN_SIZE | EGL_RED_SIZE | EGL_LUMINANCE_SIZE | EGL_ALPHA_MASK_SIZE => have >= v,
                    EGL_SAMPLES | EGL_SAMPLE_BUFFERS => have >= v,
                    EGL_RENDERABLE_TYPE | EGL_SURFACE_TYPE | EGL_CONFORMANT => {
                        // Swap-preserved is always there; a request for
                        // other bits must be satisfied.
                        have & v == v
                    }
                    EGL_CONFIG_ID => have == v,
                    EGL_COLOR_BUFFER_TYPE => have == v,
                    EGL_CONFIG_CAVEAT | EGL_TRANSPARENT_TYPE => v == EGL_NONE as i32 || have == v,
                    EGL_RECORDABLE_ANDROID => v == 0 || have != 0,
                    EGL_NATIVE_RENDERABLE | EGL_BIND_TO_TEXTURE_RGB | EGL_BIND_TO_TEXTURE_RGBA => {
                        v == 0 || v == EGL_DONT_CARE
                    }
                    EGL_LEVEL => have == v,
                    _ => true,
                }
            })
        })
        .collect();
    let requested = |a: u32| get(a).is_some_and(|v| v != 0 && v != EGL_DONT_CARE);
    found.sort_by_key(|&i| {
        let c = &CONFIGS[i as usize];
        let mut color = 0;
        for (a, v) in
            [(EGL_RED_SIZE, c.r), (EGL_GREEN_SIZE, c.g), (EGL_BLUE_SIZE, c.b), (EGL_ALPHA_SIZE, c.a)]
        {
            if requested(a) {
                color += v;
            }
        }
        (-color, c.buffer_size(), c.depth, c.stencil, i)
    });
    found
}

// Framebuffer parameters (rcGetFBParam).
pub const FB_WIDTH: u32 = 1;
pub const FB_HEIGHT: u32 = 2;
pub const FB_XDPI: u32 = 3;
pub const FB_YDPI: u32 = 4;
pub const FB_FPS: u32 = 5;
pub const FB_MIN_SWAP_INTERVAL: u32 = 6;
pub const FB_MAX_SWAP_INTERVAL: u32 = 7;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_config_like_egl() {
        // SurfaceFlinger-style: RGBA 8888, window, ES2, recordable.
        let a = [0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3033, 4, 0x3040, 4, 0x3142, 1, 0x3038];
        assert_eq!(choose_config(&a)[0], 0);
        // With depth: only the depth configs, smallest color satisfying.
        let a = [0x3025, 16, 0x3038];
        assert_eq!(choose_config(&a), [5, 3, 1], "RGB565 first: color not requested");
        let a = [0x3024, 5, 0x3023, 6, 0x3022, 5, 0x3038];
        assert_eq!(choose_config(&a)[0], 2, "larger color first when requested");
        assert_eq!(choose_config(&[0x3028, 5, 0x3038]), [4], "config id exact");
        assert!(choose_config(&[0x3031, 4, 0x3038]).is_empty(), "no multisampling");
        assert_eq!(choose_config(&[0x3147, 1, 0x3038]).len(), 6, "unknown attribute accepted");
        let p = pack_configs();
        assert_eq!(p.len(), 34 * 7);
        assert_eq!(p[34 + 4], 1, "config id of the first config");
    }
}
