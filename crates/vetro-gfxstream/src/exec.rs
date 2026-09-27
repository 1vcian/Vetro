//! The op stream the decoder hands to a [`GlExecutor`] (ADR 0036): WebGL2
//! calls with host object ids instead of guest names, executed in batches.
//!
//! Layout: a sequence of u32 words; each op starts with `code << 16 | n`,
//! `n` = words of the op including this header. Floats are their bits.
//! Bulk data (pixels, buffer contents, shader text) lives in a separate
//! byte `blob`; an op refers to it with two words (offset, length), offsets
//! 8-aligned so the executor can view it as any typed array. Ops that read
//! back ([`Code::ReadPixels`] …) write their result, in order, into the
//! `out` buffer of the batch. The JS executor (`web/app/gl.mjs`) switches on
//! the same codes (`web/app/gl-ops.mjs`, generated from [`CODES`] and checked
//! by a test).

macro_rules! codes {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {
        /// Op codes. Their numeric values are part of the contract with the
        /// JS executor.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u16)]
        pub enum Code { $($(#[$m])* $name),* }
        /// Every code with its name, in numeric order.
        pub const CODES: &[(Code, &str)] = &[$((Code::$name, stringify!($name))),*];
    };
}

codes! {
    // Objects: kind (see `Kind`), host id, extra (shader type).
    Create,
    Delete,
    // Context state.
    Enable,
    Disable,
    BlendColor,
    BlendEquationSeparate,
    BlendFuncSeparate,
    ColorMask,
    DepthMask,
    StencilMaskSeparate,
    ClearColor,
    ClearDepth,
    ClearStencil,
    CullFace,
    FrontFace,
    DepthFunc,
    DepthRange,
    LineWidth,
    PolygonOffset,
    SampleCoverage,
    Scissor,
    Viewport,
    StencilFuncSeparate,
    StencilOpSeparate,
    Hint,
    PixelStorei,
    ActiveTexture,
    BindTexture,
    BindSampler,
    UseProgram,
    BindVertexArray,
    BindBuffer,
    BindBufferRange,
    BindFramebuffer,
    BindRenderbuffer,
    BindTransformFeedback,
    VertexAttrib4f,
    VertexAttribI4i,
    VertexAttribI4ui,
    // Drawing.
    Clear,
    ClearBufferiv,
    ClearBufferuiv,
    ClearBufferfv,
    ClearBufferfi,
    DrawArrays,
    DrawElements,
    DrawArraysInstanced,
    DrawElementsInstanced,
    DrawRangeElements,
    // Buffers.
    BufferData,
    BufferSubData,
    CopyBufferSubData,
    // Textures.
    TexImage2D,
    TexSubImage2D,
    TexImage3D,
    TexSubImage3D,
    CompressedTexImage2D,
    CompressedTexSubImage2D,
    CopyTexImage2D,
    CopyTexSubImage2D,
    CopyTexSubImage3D,
    TexParameteri,
    TexParameterf,
    GenerateMipmap,
    TexStorage2D,
    TexStorage3D,
    SamplerParameteri,
    SamplerParameterf,
    // Renderbuffers and framebuffers.
    RenderbufferStorage,
    RenderbufferStorageMultisample,
    FramebufferTexture2D,
    FramebufferRenderbuffer,
    FramebufferTextureLayer,
    DrawBuffers,
    ReadBuffer,
    BlitFramebuffer,
    InvalidateFramebuffer,
    // Shaders and programs.
    ShaderSource,
    CompileShader,
    AttachShader,
    DetachShader,
    BindAttribLocation,
    LinkProgram,
    /// program, then (base, size, name offset, name length) per uniform; the
    /// names are in the blob: how the executor resolves virtual locations.
    ProgramUniforms,
    Uniformfv,
    Uniformiv,
    Uniformuiv,
    UniformMatrixfv,
    UniformBlockBinding,
    TransformFeedbackVaryings,
    // Vertex arrays.
    VertexAttribPointer,
    VertexAttribIPointer,
    EnableVertexAttribArray,
    DisableVertexAttribArray,
    VertexAttribDivisor,
    /// Client-side vertex data: uploaded to the executor's scratch buffer of
    /// that attribute, then the pointer set at offset 0 (tight packing);
    /// the last word is the array buffer to bind back.
    VertexData,
    VertexIData,
    /// Inline indices: scratch element buffer, draw, element buffer bound
    /// back (last word).
    DrawElementsData,
    DrawElementsInstancedData,
    // Queries and transform feedback.
    BeginQuery,
    EndQuery,
    BeginTransformFeedback,
    EndTransformFeedback,
    PauseTransformFeedback,
    ResumeTransformFeedback,
    Flush,
    Finish,
    // Readbacks (write into `out`).
    ReadPixels,
    GetBufferSubData,
    GetQueryResult,
    // Executor helpers on ColorBuffers and surfaces. They save and restore
    // every GL binding and pixel-store value they touch, so the decoder's
    // view of the GL state stays true.
    /// tex, internal format, width, height, format, type: storage, no data.
    TexAlloc,
    /// tex, x, y, width, height, format, type, blob: tightly packed rows.
    TexUpload,
    /// tex, x, y, width, height, format, type → `out` (tightly packed rows,
    /// bottom row first like glReadPixels).
    ReadTexture,
    /// fbo, tex (0 = none), depth-stencil renderbuffer (0 = none), width,
    /// height: a window surface's framebuffer on its current ColorBuffer.
    SurfaceAttach,
    /// tex, width, height: draws a texture onto the canvas and hands the
    /// frame to the page.
    Present,
}

/// Object kinds of [`Code::Create`] / [`Code::Delete`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Texture,
    Buffer,
    Framebuffer,
    Renderbuffer,
    Shader,
    Program,
    VertexArray,
    Sampler,
    Query,
    TransformFeedback,
}

/// Executes batches of ops.
pub trait GlExecutor {
    /// Runs `words` (with their `blob`); read ops write their results, in
    /// order, into `out`, which is exactly as long as they need.
    fn execute(&mut self, words: &[u32], blob: &[u8], out: &mut [u8]);
}

/// Executor that does nothing: readbacks read zeros. For native runs
/// without a GPU (the guest's graphics still work, nothing is drawn).
#[derive(Default)]
pub struct NullExecutor {
    /// Batches and ops seen.
    pub batches: u64,
    pub ops: u64,
}

impl GlExecutor for NullExecutor {
    fn execute(&mut self, words: &[u32], _blob: &[u8], _out: &mut [u8]) {
        self.batches += 1;
        let mut i = 0;
        while i < words.len() {
            let n = (words[i] & 0xffff).max(1) as usize;
            self.ops += 1;
            i += n;
        }
    }
}

/// Records every batch (for replay in a browser, `tools/gfxstream`), then
/// hands it to an inner executor. Format: per batch `u32 words, u32 blob
/// bytes, u32 out bytes`, the words, the blob, then the `out` bytes the inner
/// executor produced.
pub struct Recorder<E: GlExecutor> {
    pub inner: E,
    pub log: Vec<u8>,
}

impl<E: GlExecutor> Recorder<E> {
    pub fn new(inner: E) -> Self {
        Self { inner, log: Vec::new() }
    }
}

impl<E: GlExecutor> GlExecutor for Recorder<E> {
    fn execute(&mut self, words: &[u32], blob: &[u8], out: &mut [u8]) {
        self.inner.execute(words, blob, out);
        for n in [words.len(), blob.len(), out.len()] {
            self.log.extend_from_slice(&(n as u32).to_le_bytes());
        }
        for w in words {
            self.log.extend_from_slice(&w.to_le_bytes());
        }
        self.log.extend_from_slice(blob);
        self.log.extend_from_slice(out);
    }
}

/// Batch under construction.
#[derive(Default)]
pub struct OpBuf {
    pub words: Vec<u32>,
    pub blob: Vec<u8>,
    /// Bytes the read ops of this batch will write.
    pub out_len: usize,
}

impl OpBuf {
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn clear(&mut self) {
        self.words.clear();
        self.blob.clear();
        self.out_len = 0;
    }

    fn header(&mut self, code: Code, n: usize) {
        debug_assert!(n < 0x10000);
        self.words.push(((code as u32) << 16) | n as u32);
    }

    /// An op with word arguments.
    pub fn op(&mut self, code: Code, args: &[u32]) {
        self.header(code, 1 + args.len());
        self.words.extend_from_slice(args);
    }

    /// Adds bytes to the blob; returns (offset, length).
    pub fn blob(&mut self, data: &[u8]) -> (u32, u32) {
        let off = self.blob.len().next_multiple_of(8);
        self.blob.resize(off, 0);
        self.blob.extend_from_slice(data);
        (off as u32, data.len() as u32)
    }

    /// An op whose last two words are a blob reference to `data`.
    pub fn op_blob(&mut self, code: Code, args: &[u32], data: &[u8]) {
        let (off, len) = self.blob(data);
        self.header(code, 3 + args.len());
        self.words.extend_from_slice(args);
        self.words.push(off);
        self.words.push(len);
    }

    /// Like [`OpBuf::op_blob`] with words after the blob reference.
    pub fn op_blob_then(&mut self, code: Code, args: &[u32], data: &[u8], tail: &[u32]) {
        let (off, len) = self.blob(data);
        self.header(code, 3 + args.len() + tail.len());
        self.words.extend_from_slice(args);
        self.words.push(off);
        self.words.push(len);
        self.words.extend_from_slice(tail);
    }

    /// A read op producing `n` bytes of `out`.
    pub fn op_read(&mut self, code: Code, args: &[u32], n: usize) {
        self.op(code, args);
        self.out_len += n;
    }
}

/// f32 as an op word.
pub fn fw(v: f32) -> u32 {
    v.to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `web/app/gl-ops.mjs` must list the codes in the same order.
    #[test]
    fn js_codes_match() {
        let mut js = String::from(
            "// Generated from crates/vetro-gfxstream/src/exec.rs (CODES); checked by its tests.\n\
             // Op codes of the WebGL2 op stream (ADR 0036).\n",
        );
        for (i, (_, name)) in CODES.iter().enumerate() {
            js.push_str(&format!("export const {name} = {i};\n"));
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/app/gl-ops.mjs");
        if std::env::var_os("VETRO_UPDATE_GL_OPS").is_some() {
            std::fs::write(path, &js).unwrap();
        }
        let have = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            have, js,
            "web/app/gl-ops.mjs is stale: VETRO_UPDATE_GL_OPS=1 cargo test -p vetro-gfxstream"
        );
    }

    #[test]
    fn layout_of_ops_and_blobs() {
        let mut b = OpBuf::default();
        b.op(Code::Viewport, &[0, 0, 640, 480]);
        b.op_blob(Code::BufferData, &[0x8892, 0x88E4], &[1, 2, 3]);
        b.op_blob(Code::BufferSubData, &[0x8892, 4], &[9]);
        b.op_read(Code::ReadPixels, &[0, 0, 1, 1, 0x1908, 0x1401], 4);
        assert_eq!(b.words[0], (Code::Viewport as u32) << 16 | 5);
        assert_eq!(b.words[5], (Code::BufferData as u32) << 16 | 5);
        assert_eq!(&b.words[8..10], &[0, 3]);
        assert_eq!(&b.words[13..15], &[8, 1], "8-aligned blob offsets");
        assert_eq!(b.out_len, 4);
        let mut n = NullExecutor::default();
        n.execute(&b.words, &b.blob, &mut [0; 4]);
        assert_eq!((n.batches, n.ops), (1, 4));
    }
}
