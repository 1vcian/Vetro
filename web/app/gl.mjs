// WebGL2 executor of the gfxstream op stream (ADR 0037). The decoder in
// vetro-gfxstream (Rust, inside vetro-wasm) turns the guest's GLES calls into
// ops with host object ids (crates/vetro-gfxstream/src/exec.rs); this runs
// them on one WebGL2 context of an OffscreenCanvas owned by the Worker, in
// batches. Read ops write their results into `out`, in order. Presenting a
// scanout draws its texture on the canvas and hands the frame to the page as
// an ImageBitmap (no readback).
//
// Everything the guest can query was answered by the decoder; the only
// results that come from here are the readbacks (ReadPixels, ReadTexture,
// GetBufferSubData, GetQueryResult).

import * as O from './gl-ops.mjs';

const KIND = ['texture', 'buffer', 'framebuffer', 'renderbuffer', 'shader', 'program', 'vertexArray', 'sampler', 'query', 'transformFeedback'];

// GL enums used here.
const GL = {
  TEXTURE_2D: 0x0DE1, TEXTURE0: 0x84C0, ARRAY_BUFFER: 0x8892, ELEMENT_ARRAY_BUFFER: 0x8893,
  STREAM_DRAW: 0x88E0, FRAMEBUFFER: 0x8D40, READ_FRAMEBUFFER: 0x8CA8, DRAW_FRAMEBUFFER: 0x8CA9,
  RENDERBUFFER: 0x8D41, COLOR_ATTACHMENT0: 0x8CE0, DEPTH_STENCIL_ATTACHMENT: 0x821A,
  DEPTH24_STENCIL8: 0x88F0, PIXEL_PACK_BUFFER: 0x88EB, PIXEL_UNPACK_BUFFER: 0x88EC,
  UNPACK_ALIGNMENT: 0x0CF5, UNPACK_ROW_LENGTH: 0x0CF2, UNPACK_SKIP_ROWS: 0x0CF3, UNPACK_SKIP_PIXELS: 0x0CF4,
  PACK_ALIGNMENT: 0x0D05, PACK_ROW_LENGTH: 0x0D02, PACK_SKIP_ROWS: 0x0D03, PACK_SKIP_PIXELS: 0x0D04,
  TEXTURE_MIN_FILTER: 0x2801, TEXTURE_MAG_FILTER: 0x2800, TEXTURE_WRAP_S: 0x2802, TEXTURE_WRAP_T: 0x2803,
  LINEAR: 0x2601, NEAREST: 0x2600, CLAMP_TO_EDGE: 0x812F, TRIANGLES: 4, RGBA: 0x1908, UNSIGNED_BYTE: 0x1401,
  BLEND: 0x0BE2, CULL_FACE: 0x0B44, DEPTH_TEST: 0x0B71, SCISSOR_TEST: 0x0C11, STENCIL_TEST: 0x0B90,
  RASTERIZER_DISCARD: 0x8C89, QUERY_RESULT: 0x8866,
};

const BLIT_VS = `#version 300 es
out vec2 uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = p;
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;
const BLIT_FS = `#version 300 es
precision mediump float;
uniform sampler2D tex;
in vec2 uv;
out vec4 color;
void main() { color = vec4(texture(tex, uv).rgb, 1.0); }`;

/** The typed array a pixel `type` needs (WebGL checks it). */
function pixelArray(type, bytes) {
  const b = bytes.buffer, o = bytes.byteOffset, n = bytes.byteLength;
  const aligned = (k) => (o % k === 0 ? [b, o] : [bytes.slice().buffer, 0]);
  switch (type) {
    case 0x1400: return new Int8Array(b, o, n);
    case 0x1402: { const [bb, oo] = aligned(2); return new Int16Array(bb, oo, n >> 1); }
    case 0x1403: case 0x8363: case 0x8033: case 0x8034: case 0x140B: { const [bb, oo] = aligned(2); return new Uint16Array(bb, oo, n >> 1); }
    case 0x1404: { const [bb, oo] = aligned(4); return new Int32Array(bb, oo, n >> 2); }
    case 0x1406: { const [bb, oo] = aligned(4); return new Float32Array(bb, oo, n >> 2); }
    case 0x1405: case 0x8368: case 0x84FA: case 0x8C3B: case 0x8C3E: { const [bb, oo] = aligned(4); return new Uint32Array(bb, oo, n >> 2); }
    default: return new Uint8Array(b, o, n);
  }
}

export class WebGlExecutor {
  /**
   * @param {OffscreenCanvas} canvas
   * @param {(bitmap: ImageBitmap, width: number, height: number) => void} onFrame
   */
  constructor(canvas, onFrame) {
    this.canvas = canvas;
    this.onFrame = onFrame;
    const gl = canvas.getContext('webgl2', {
      alpha: false, antialias: false, depth: false, stencil: false,
      premultipliedAlpha: false, preserveDrawingBuffer: false, powerPreference: 'high-performance',
    });
    if (!gl) throw new Error('WebGL2 is not available');
    this.gl = gl;
    gl.getExtension('EXT_color_buffer_float');
    gl.getExtension('EXT_color_buffer_half_float');
    this.objs = [null];
    this.programs = new Map(); // id -> { uniforms: [{base, size, name, array}], cache: Map }
    this.program = 0;
    this.scratch = [];
    this.scratchElements = gl.createBuffer();
    this.readFbo = gl.createFramebuffer();
    this.rbSizes = new Map();
    this.stats = { batches: 0, ops: 0, frames: 0, errors: 0 };
    this.log = [];
    const vs = this.#compile(gl.VERTEX_SHADER, BLIT_VS);
    const fs = this.#compile(gl.FRAGMENT_SHADER, BLIT_FS);
    this.blit = gl.createProgram();
    gl.attachShader(this.blit, vs);
    gl.attachShader(this.blit, fs);
    gl.linkProgram(this.blit);
    this.blitTex = gl.getUniformLocation(this.blit, 'tex');
    this.blitVao = gl.createVertexArray();
    this.blitSampler = gl.createSampler();
    gl.samplerParameteri(this.blitSampler, GL.TEXTURE_MIN_FILTER, GL.NEAREST);
    gl.samplerParameteri(this.blitSampler, GL.TEXTURE_MAG_FILTER, GL.NEAREST);
  }

  /** The limits the fixed profile promises (crates/vetro-gfxstream/src/caps.rs). */
  static check(gl) {
    const need = [[gl.MAX_TEXTURE_SIZE, 4096], [gl.MAX_RENDERBUFFER_SIZE, 4096], [gl.MAX_VERTEX_ATTRIBS, 16],
      [gl.MAX_TEXTURE_IMAGE_UNITS, 16], [gl.MAX_COMBINED_TEXTURE_IMAGE_UNITS, 32], [gl.MAX_DRAW_BUFFERS, 4],
      [gl.MAX_VERTEX_UNIFORM_VECTORS, 256], [gl.MAX_FRAGMENT_UNIFORM_VECTORS, 224], [gl.MAX_VARYING_VECTORS, 15]];
    return need.filter(([p, v]) => gl.getParameter(p) < v).map(([p, v]) => `0x${p.toString(16)} < ${v}`);
  }

  #compile(type, src) {
    const gl = this.gl;
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    return s;
  }

  #warn(what) {
    this.stats.errors++;
    if (this.log.length < 200) this.log.push(what);
  }

  #obj(id) { return id ? this.objs[id] ?? null : null; }

  #location(loc) {
    const p = this.programs.get(this.program);
    if (!p) return null;
    let l = p.cache.get(loc);
    if (l !== undefined) return l;
    l = null;
    for (const u of p.uniforms) {
      if (loc >= u.base && loc < u.base + u.size) {
        const name = u.array ? `${u.name}[${loc - u.base}]` : u.name;
        l = this.gl.getUniformLocation(this.objs[this.program], name);
        break;
      }
    }
    p.cache.set(loc, l);
    return l;
  }

  /**
   * Runs one batch.
   * @param {Uint32Array} w  the op words
   * @param {Uint8Array} blob
   * @param {Uint8Array} out  read results, in order
   */
  execute(w, blob, out) {
    const gl = this.gl;
    const f = new Float32Array(w.buffer, w.byteOffset, w.length);
    const i32 = new Int32Array(w.buffer, w.byteOffset, w.length);
    const bytes = (off, len) => blob.subarray(off, off + len);
    const f32s = (off, len) => pixelArray(0x1406, bytes(off, len));
    const i32s = (off, len) => pixelArray(0x1404, bytes(off, len));
    const u32s = (off, len) => pixelArray(0x1405, bytes(off, len));
    const text = (off, len) => new TextDecoder().decode(bytes(off, len));
    let o = 0;
    this.stats.batches++;
    for (let i = 0; i < w.length;) {
      const h = w[i], code = h >>> 16, n = h & 0xffff, a = i + 1;
      if (n === 0) break;
      this.stats.ops++;
      try {
        switch (code) {
          case O.Create: {
            const kind = KIND[w[a]], id = w[a + 1];
            let x = null;
            switch (kind) {
              case 'texture': x = gl.createTexture(); break;
              case 'buffer': x = gl.createBuffer(); break;
              case 'framebuffer': x = gl.createFramebuffer(); break;
              case 'renderbuffer': x = gl.createRenderbuffer(); break;
              case 'shader': x = gl.createShader(w[a + 2]); break;
              case 'program': x = gl.createProgram(); this.programs.set(id, { uniforms: [], cache: new Map() }); break;
              case 'vertexArray': x = gl.createVertexArray(); break;
              case 'sampler': x = gl.createSampler(); break;
              case 'query': x = gl.createQuery(); break;
              case 'transformFeedback': x = gl.createTransformFeedback(); break;
            }
            this.objs[id] = x;
            break;
          }
          case O.Delete: {
            const kind = KIND[w[a]], id = w[a + 1], x = this.objs[id];
            if (x) {
              const del = { texture: 'deleteTexture', buffer: 'deleteBuffer', framebuffer: 'deleteFramebuffer',
                renderbuffer: 'deleteRenderbuffer', shader: 'deleteShader', program: 'deleteProgram',
                vertexArray: 'deleteVertexArray', sampler: 'deleteSampler', query: 'deleteQuery',
                transformFeedback: 'deleteTransformFeedback' }[kind];
              gl[del](x);
            }
            this.objs[id] = null;
            this.programs.delete(id);
            break;
          }
          case O.Enable: gl.enable(w[a]); break;
          case O.Disable: gl.disable(w[a]); break;
          case O.BlendColor: gl.blendColor(f[a], f[a + 1], f[a + 2], f[a + 3]); break;
          case O.BlendEquationSeparate: gl.blendEquationSeparate(w[a], w[a + 1]); break;
          case O.BlendFuncSeparate: gl.blendFuncSeparate(w[a], w[a + 1], w[a + 2], w[a + 3]); break;
          case O.ColorMask: gl.colorMask(!!w[a], !!w[a + 1], !!w[a + 2], !!w[a + 3]); break;
          case O.DepthMask: gl.depthMask(!!w[a]); break;
          case O.StencilMaskSeparate: gl.stencilMaskSeparate(w[a], w[a + 1]); break;
          case O.ClearColor: gl.clearColor(f[a], f[a + 1], f[a + 2], f[a + 3]); break;
          case O.ClearDepth: gl.clearDepth(f[a]); break;
          case O.ClearStencil: gl.clearStencil(i32[a]); break;
          case O.CullFace: gl.cullFace(w[a]); break;
          case O.FrontFace: gl.frontFace(w[a]); break;
          case O.DepthFunc: gl.depthFunc(w[a]); break;
          case O.DepthRange: gl.depthRange(f[a], f[a + 1]); break;
          case O.LineWidth: gl.lineWidth(f[a]); break;
          case O.PolygonOffset: gl.polygonOffset(f[a], f[a + 1]); break;
          case O.SampleCoverage: gl.sampleCoverage(f[a], !!w[a + 1]); break;
          case O.Scissor: gl.scissor(i32[a], i32[a + 1], i32[a + 2], i32[a + 3]); break;
          case O.Viewport: gl.viewport(i32[a], i32[a + 1], i32[a + 2], i32[a + 3]); break;
          case O.StencilFuncSeparate: gl.stencilFuncSeparate(w[a], w[a + 1], i32[a + 2], w[a + 3]); break;
          case O.StencilOpSeparate: gl.stencilOpSeparate(w[a], w[a + 1], w[a + 2], w[a + 3]); break;
          case O.Hint: gl.hint(w[a], w[a + 1]); break;
          case O.PixelStorei: gl.pixelStorei(w[a], i32[a + 1]); break;
          case O.ActiveTexture: gl.activeTexture(w[a]); break;
          case O.BindTexture: gl.bindTexture(w[a], this.#obj(w[a + 1])); break;
          case O.BindSampler: gl.bindSampler(w[a], this.#obj(w[a + 1])); break;
          case O.UseProgram: gl.useProgram(this.#obj(w[a])); this.program = w[a]; break;
          case O.BindVertexArray: gl.bindVertexArray(this.#obj(w[a])); break;
          case O.BindBuffer: gl.bindBuffer(w[a], this.#obj(w[a + 1])); break;
          case O.BindBufferRange:
            if (w[a + 4] === 0) gl.bindBufferBase(w[a], w[a + 1], this.#obj(w[a + 2]));
            else gl.bindBufferRange(w[a], w[a + 1], this.#obj(w[a + 2]), w[a + 3], w[a + 4]);
            break;
          case O.BindFramebuffer: gl.bindFramebuffer(w[a], this.#obj(w[a + 1])); break;
          case O.BindRenderbuffer: gl.bindRenderbuffer(GL.RENDERBUFFER, this.#obj(w[a])); break;
          case O.BindTransformFeedback: gl.bindTransformFeedback(0x8E22, this.#obj(w[a])); break;
          case O.VertexAttrib4f: gl.vertexAttrib4f(w[a], f[a + 1], f[a + 2], f[a + 3], f[a + 4]); break;
          case O.VertexAttribI4i: gl.vertexAttribI4i(w[a], i32[a + 1], i32[a + 2], i32[a + 3], i32[a + 4]); break;
          case O.VertexAttribI4ui: gl.vertexAttribI4ui(w[a], w[a + 1], w[a + 2], w[a + 3], w[a + 4]); break;
          case O.Clear: gl.clear(w[a]); break;
          case O.ClearBufferiv: gl.clearBufferiv(w[a], i32[a + 1], i32s(w[a + 2], w[a + 3])); break;
          case O.ClearBufferuiv: gl.clearBufferuiv(w[a], i32[a + 1], u32s(w[a + 2], w[a + 3])); break;
          case O.ClearBufferfv: gl.clearBufferfv(w[a], i32[a + 1], f32s(w[a + 2], w[a + 3])); break;
          case O.ClearBufferfi: gl.clearBufferfi(w[a], i32[a + 1], f[a + 2], i32[a + 3]); break;
          case O.DrawArrays: gl.drawArrays(w[a], i32[a + 1], i32[a + 2]); break;
          case O.DrawElements: gl.drawElements(w[a], i32[a + 1], w[a + 2], w[a + 3]); break;
          case O.DrawArraysInstanced: gl.drawArraysInstanced(w[a], i32[a + 1], i32[a + 2], i32[a + 3]); break;
          case O.DrawElementsInstanced: gl.drawElementsInstanced(w[a], i32[a + 1], w[a + 2], w[a + 3], i32[a + 4]); break;
          case O.DrawRangeElements: gl.drawRangeElements(w[a], w[a + 1], w[a + 2], i32[a + 3], w[a + 4], w[a + 5]); break;
          case O.BufferData: {
            // target, size, blob, usage
            const len = w[a + 3];
            if (len) gl.bufferData(w[a], bytes(w[a + 2], len), w[a + 4]);
            else gl.bufferData(w[a], w[a + 1], w[a + 4]);
            break;
          }
          case O.BufferSubData: gl.bufferSubData(w[a], w[a + 1], bytes(w[a + 2], w[a + 3])); break;
          case O.CopyBufferSubData: gl.copyBufferSubData(w[a], w[a + 1], w[a + 2], w[a + 3], w[a + 4]); break;
          case O.TexImage2D: {
            const [t, lv, ifmt, wd, ht, b, fmt, ty, pbo, off, len] = w.subarray(a, a + 11);
            if (pbo) gl.texImage2D(t, lv, ifmt, wd, ht, b, fmt, ty, pbo - 1);
            else gl.texImage2D(t, lv, ifmt, wd, ht, b, fmt, ty, len ? pixelArray(ty, bytes(off, len)) : null);
            break;
          }
          case O.TexSubImage2D: {
            const [t, lv, x, y, wd, ht, fmt, ty, pbo, off, len] = w.subarray(a, a + 11);
            if (pbo) gl.texSubImage2D(t, lv, x, y, wd, ht, fmt, ty, pbo - 1);
            else if (len) gl.texSubImage2D(t, lv, x, y, wd, ht, fmt, ty, pixelArray(ty, bytes(off, len)));
            break;
          }
          case O.TexImage3D: {
            const [t, lv, ifmt, wd, ht, dp, b, fmt, ty, pbo, off, len] = w.subarray(a, a + 12);
            if (pbo) gl.texImage3D(t, lv, ifmt, wd, ht, dp, b, fmt, ty, pbo - 1);
            else gl.texImage3D(t, lv, ifmt, wd, ht, dp, b, fmt, ty, len ? pixelArray(ty, bytes(off, len)) : null);
            break;
          }
          case O.TexSubImage3D: {
            const [t, lv, x, y, z, wd, ht, dp, fmt, ty, pbo, off, len] = w.subarray(a, a + 13);
            if (pbo) gl.texSubImage3D(t, lv, x, y, z, wd, ht, dp, fmt, ty, pbo - 1);
            else if (len) gl.texSubImage3D(t, lv, x, y, z, wd, ht, dp, fmt, ty, pixelArray(ty, bytes(off, len)));
            break;
          }
          case O.CompressedTexImage2D: {
            const [t, lv, ifmt, wd, ht, b, off, len] = w.subarray(a, a + 8);
            gl.compressedTexImage2D(t, lv, ifmt, wd, ht, b, bytes(off, len));
            break;
          }
          case O.CompressedTexSubImage2D: {
            const [t, lv, x, y, wd, ht, fmt, off, len] = w.subarray(a, a + 9);
            gl.compressedTexSubImage2D(t, lv, x, y, wd, ht, fmt, bytes(off, len));
            break;
          }
          case O.CopyTexImage2D: gl.copyTexImage2D(w[a], i32[a + 1], w[a + 2], i32[a + 3], i32[a + 4], i32[a + 5], i32[a + 6], i32[a + 7]); break;
          case O.CopyTexSubImage2D: gl.copyTexSubImage2D(w[a], i32[a + 1], i32[a + 2], i32[a + 3], i32[a + 4], i32[a + 5], i32[a + 6], i32[a + 7]); break;
          case O.CopyTexSubImage3D: gl.copyTexSubImage3D(w[a], i32[a + 1], i32[a + 2], i32[a + 3], i32[a + 4], i32[a + 5], i32[a + 6], i32[a + 7], i32[a + 8]); break;
          case O.TexParameteri: gl.texParameteri(w[a], w[a + 1], i32[a + 2]); break;
          case O.TexParameterf: gl.texParameterf(w[a], w[a + 1], f[a + 2]); break;
          case O.GenerateMipmap: gl.generateMipmap(w[a]); break;
          case O.TexStorage2D: gl.texStorage2D(w[a], i32[a + 1], w[a + 2], i32[a + 3], i32[a + 4]); break;
          case O.TexStorage3D: gl.texStorage3D(w[a], i32[a + 1], w[a + 2], i32[a + 3], i32[a + 4], i32[a + 5]); break;
          case O.SamplerParameteri: gl.samplerParameteri(this.#obj(w[a]), w[a + 1], i32[a + 2]); break;
          case O.SamplerParameterf: gl.samplerParameterf(this.#obj(w[a]), w[a + 1], f[a + 2]); break;
          case O.RenderbufferStorage: gl.renderbufferStorage(GL.RENDERBUFFER, w[a], i32[a + 1], i32[a + 2]); break;
          case O.RenderbufferStorageMultisample: gl.renderbufferStorageMultisample(GL.RENDERBUFFER, i32[a], w[a + 1], i32[a + 2], i32[a + 3]); break;
          case O.FramebufferTexture2D: gl.framebufferTexture2D(w[a], w[a + 1], w[a + 2], this.#obj(w[a + 3]), i32[a + 4]); break;
          case O.FramebufferRenderbuffer: gl.framebufferRenderbuffer(w[a], w[a + 1], w[a + 2], this.#obj(w[a + 3])); break;
          case O.FramebufferTextureLayer: gl.framebufferTextureLayer(w[a], w[a + 1], this.#obj(w[a + 2]), i32[a + 3], i32[a + 4]); break;
          case O.DrawBuffers: gl.drawBuffers(Array.from(w.subarray(a, i + n))); break;
          case O.ReadBuffer: gl.readBuffer(w[a]); break;
          case O.BlitFramebuffer: gl.blitFramebuffer(i32[a], i32[a + 1], i32[a + 2], i32[a + 3], i32[a + 4], i32[a + 5], i32[a + 6], i32[a + 7], w[a + 8], w[a + 9]); break;
          case O.InvalidateFramebuffer: gl.invalidateFramebuffer(w[a], Array.from(w.subarray(a + 1, i + n))); break;
          case O.ShaderSource: gl.shaderSource(this.#obj(w[a]), text(w[a + 1], w[a + 2])); break;
          case O.CompileShader: {
            const s = this.#obj(w[a]);
            gl.compileShader(s);
            break;
          }
          case O.AttachShader: gl.attachShader(this.#obj(w[a]), this.#obj(w[a + 1])); break;
          case O.DetachShader: gl.detachShader(this.#obj(w[a]), this.#obj(w[a + 1])); break;
          case O.BindAttribLocation: gl.bindAttribLocation(this.#obj(w[a]), w[a + 1], text(w[a + 2], w[a + 3])); break;
          case O.LinkProgram: {
            const p = this.#obj(w[a]);
            gl.linkProgram(p);
            // Host compiler messages stay here (the guest was told "success").
            if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
              this.#warn(`link of program ${w[a]} failed: ${gl.getProgramInfoLog(p)}`);
              for (const s of gl.getAttachedShaders(p) ?? []) {
                if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) this.#warn(`  shader: ${gl.getShaderInfoLog(s)}`);
              }
            }
            break;
          }
          case O.ProgramUniforms: {
            const id = w[a];
            const count = (n - 4) / 5;
            const [boff, blen] = [w[i + n - 2], w[i + n - 1]];
            const names = bytes(boff, blen);
            const uniforms = [];
            for (let k = 0; k < count; k++) {
              const b = a + 1 + 5 * k;
              uniforms.push({ base: w[b], size: w[b + 1], name: new TextDecoder().decode(names.subarray(w[b + 2], w[b + 2] + w[b + 3])), array: !!w[b + 4] });
            }
            this.programs.set(id, { uniforms, cache: new Map() });
            break;
          }
          case O.Uniformfv: case O.Uniformiv: case O.Uniformuiv: {
            const [loc, , comps, off, len] = w.subarray(a, a + 5);
            const l = this.#location(loc);
            if (!l) break;
            const data = code === O.Uniformfv ? f32s(off, len) : code === O.Uniformiv ? i32s(off, len) : u32s(off, len);
            const fn = ['', '1', '2', '3', '4'][comps] + (code === O.Uniformfv ? 'fv' : code === O.Uniformiv ? 'iv' : 'uiv');
            gl['uniform' + fn](l, data);
            break;
          }
          case O.UniformMatrixfv: {
            const [loc, , , c, r, tr, off, len] = w.subarray(a, a + 8);
            const l = this.#location(loc);
            if (!l) break;
            const name = c === r ? `uniformMatrix${c}fv` : `uniformMatrix${c}x${r}fv`;
            gl[name](l, !!tr, f32s(off, len));
            break;
          }
          case O.UniformBlockBinding: gl.uniformBlockBinding(this.#obj(w[a]), w[a + 1], w[a + 2]); break;
          case O.VertexAttribPointer: gl.vertexAttribPointer(w[a], i32[a + 1], w[a + 2], !!w[a + 3], i32[a + 4], w[a + 5]); break;
          case O.VertexAttribIPointer: gl.vertexAttribIPointer(w[a], i32[a + 1], w[a + 2], i32[a + 3], w[a + 4]); break;
          case O.EnableVertexAttribArray: gl.enableVertexAttribArray(w[a]); break;
          case O.DisableVertexAttribArray: gl.disableVertexAttribArray(w[a]); break;
          case O.VertexAttribDivisor: gl.vertexAttribDivisor(w[a], w[a + 1]); break;
          case O.VertexData: case O.VertexIData: {
            const idx = w[a];
            const int = code === O.VertexIData;
            const [off, len, restore] = int ? w.subarray(a + 3, a + 6) : w.subarray(a + 4, a + 7);
            const buf = this.scratch[idx] ??= gl.createBuffer();
            gl.bindBuffer(GL.ARRAY_BUFFER, buf);
            gl.bufferData(GL.ARRAY_BUFFER, bytes(off, len), GL.STREAM_DRAW);
            if (int) gl.vertexAttribIPointer(idx, i32[a + 1], w[a + 2], 0, 0);
            else gl.vertexAttribPointer(idx, i32[a + 1], w[a + 2], !!w[a + 3], 0, 0);
            gl.bindBuffer(GL.ARRAY_BUFFER, this.#obj(restore));
            break;
          }
          case O.DrawElementsData: case O.DrawElementsInstancedData: {
            const inst = code === O.DrawElementsInstancedData;
            const [off, len, restore] = inst ? w.subarray(a + 4, a + 7) : w.subarray(a + 3, a + 6);
            gl.bindBuffer(GL.ELEMENT_ARRAY_BUFFER, this.scratchElements);
            gl.bufferData(GL.ELEMENT_ARRAY_BUFFER, bytes(off, len), GL.STREAM_DRAW);
            if (inst) gl.drawElementsInstanced(w[a], i32[a + 1], w[a + 2], 0, i32[a + 3]);
            else gl.drawElements(w[a], i32[a + 1], w[a + 2], 0);
            gl.bindBuffer(GL.ELEMENT_ARRAY_BUFFER, this.#obj(restore));
            break;
          }
          case O.BeginQuery: gl.beginQuery(w[a], this.#obj(w[a + 1])); break;
          case O.EndQuery: gl.endQuery(w[a]); break;
          case O.BeginTransformFeedback: gl.beginTransformFeedback(w[a]); break;
          case O.EndTransformFeedback: gl.endTransformFeedback(); break;
          case O.PauseTransformFeedback: gl.pauseTransformFeedback(); break;
          case O.ResumeTransformFeedback: gl.resumeTransformFeedback(); break;
          case O.Flush: gl.flush(); break;
          case O.Finish: break; // WebGL's finish would only stall; results are read explicitly
          case O.ReadPixels: {
            const [x, y, wd, ht, fmt, ty, pbo, len] = w.subarray(a, a + 8);
            if (pbo) gl.readPixels(x, y, wd, ht, fmt, ty, pbo - 1);
            else { gl.readPixels(x, y, wd, ht, fmt, ty, pixelArray(ty, out.subarray(o, o + len))); o += len; }
            break;
          }
          case O.GetBufferSubData: {
            const [t, off, len] = w.subarray(a, a + 3);
            gl.getBufferSubData(t, off, out.subarray(o, o + len));
            o += len;
            break;
          }
          case O.GetQueryResult: {
            // WebGL makes results available only after the task ends: the
            // synchronous answer is 0 (see docs/specs/gfxstream.md).
            new DataView(out.buffer, out.byteOffset + o, 4).setUint32(0, 0, true);
            o += 4;
            break;
          }
          case O.TexAlloc: this.#texAlloc(...w.subarray(a, a + 6)); break;
          case O.TexUpload: {
            const [tex, x, y, wd, ht, fmt, ty, off, len] = w.subarray(a, a + 9);
            this.#texUpload(tex, x, y, wd, ht, fmt, ty, pixelArray(ty, bytes(off, len)));
            break;
          }
          case O.ReadTexture: {
            const [tex, x, y, wd, ht, fmt, ty, len] = w.subarray(a, a + 8);
            this.#readTexture(tex, x, y, wd, ht, fmt, ty, out.subarray(o, o + len));
            o += len;
            break;
          }
          case O.SurfaceAttach: this.#surfaceAttach(...w.subarray(a, a + 5)); break;
          case O.Present: this.#present(w[a], w[a + 1], w[a + 2]); break;
          default: this.#warn(`unknown op ${code}`);
        }
      } catch (e) {
        this.#warn(`op ${code}: ${e.message}`);
      }
      i += n;
    }
  }

  #texAlloc(tex, ifmt, width, height, fmt, ty) {
    const gl = this.gl;
    const prev = gl.getParameter(gl.TEXTURE_BINDING_2D);
    const pbo = gl.getParameter(gl.PIXEL_UNPACK_BUFFER_BINDING);
    if (pbo) gl.bindBuffer(GL.PIXEL_UNPACK_BUFFER, null);
    gl.bindTexture(GL.TEXTURE_2D, this.objs[tex]);
    gl.texImage2D(GL.TEXTURE_2D, 0, ifmt, width, height, 0, fmt, ty, null);
    gl.texParameteri(GL.TEXTURE_2D, GL.TEXTURE_MIN_FILTER, GL.LINEAR);
    gl.texParameteri(GL.TEXTURE_2D, GL.TEXTURE_MAG_FILTER, GL.LINEAR);
    gl.texParameteri(GL.TEXTURE_2D, GL.TEXTURE_WRAP_S, GL.CLAMP_TO_EDGE);
    gl.texParameteri(GL.TEXTURE_2D, GL.TEXTURE_WRAP_T, GL.CLAMP_TO_EDGE);
    gl.bindTexture(GL.TEXTURE_2D, prev);
    if (pbo) gl.bindBuffer(GL.PIXEL_UNPACK_BUFFER, pbo);
  }

  #texUpload(tex, x, y, width, height, fmt, ty, data) {
    const gl = this.gl;
    const prev = gl.getParameter(gl.TEXTURE_BINDING_2D);
    const pbo = gl.getParameter(gl.PIXEL_UNPACK_BUFFER_BINDING);
    const keys = [GL.UNPACK_ALIGNMENT, GL.UNPACK_ROW_LENGTH, GL.UNPACK_SKIP_ROWS, GL.UNPACK_SKIP_PIXELS];
    const saved = keys.map((k) => gl.getParameter(k));
    if (pbo) gl.bindBuffer(GL.PIXEL_UNPACK_BUFFER, null);
    gl.pixelStorei(GL.UNPACK_ALIGNMENT, 1);
    for (const k of keys.slice(1)) gl.pixelStorei(k, 0);
    gl.bindTexture(GL.TEXTURE_2D, this.objs[tex]);
    gl.texSubImage2D(GL.TEXTURE_2D, 0, x, y, width, height, fmt, ty, data);
    gl.bindTexture(GL.TEXTURE_2D, prev);
    keys.forEach((k, i) => gl.pixelStorei(k, saved[i]));
    if (pbo) gl.bindBuffer(GL.PIXEL_UNPACK_BUFFER, pbo);
  }

  #readTexture(tex, x, y, width, height, fmt, ty, dst) {
    const gl = this.gl;
    const prevFb = gl.getParameter(gl.READ_FRAMEBUFFER_BINDING);
    const pbo = gl.getParameter(gl.PIXEL_PACK_BUFFER_BINDING);
    const keys = [GL.PACK_ALIGNMENT, GL.PACK_ROW_LENGTH, GL.PACK_SKIP_ROWS, GL.PACK_SKIP_PIXELS];
    const saved = keys.map((k) => gl.getParameter(k));
    if (pbo) gl.bindBuffer(GL.PIXEL_PACK_BUFFER, null);
    gl.pixelStorei(GL.PACK_ALIGNMENT, 1);
    for (const k of keys.slice(1)) gl.pixelStorei(k, 0);
    gl.bindFramebuffer(GL.READ_FRAMEBUFFER, this.readFbo);
    gl.framebufferTexture2D(GL.READ_FRAMEBUFFER, GL.COLOR_ATTACHMENT0, GL.TEXTURE_2D, this.objs[tex], 0);
    gl.readPixels(x, y, width, height, fmt, ty, pixelArray(ty, dst));
    gl.framebufferTexture2D(GL.READ_FRAMEBUFFER, GL.COLOR_ATTACHMENT0, GL.TEXTURE_2D, null, 0);
    gl.bindFramebuffer(GL.READ_FRAMEBUFFER, prevFb);
    keys.forEach((k, i) => gl.pixelStorei(k, saved[i]));
    if (pbo) gl.bindBuffer(GL.PIXEL_PACK_BUFFER, pbo);
  }

  #surfaceAttach(fbo, tex, rb, width, height) {
    const gl = this.gl;
    const prevFb = gl.getParameter(gl.DRAW_FRAMEBUFFER_BINDING);
    const prevRb = gl.getParameter(gl.RENDERBUFFER_BINDING);
    gl.bindFramebuffer(GL.DRAW_FRAMEBUFFER, this.objs[fbo]);
    gl.framebufferTexture2D(GL.DRAW_FRAMEBUFFER, GL.COLOR_ATTACHMENT0, GL.TEXTURE_2D, this.#obj(tex), 0);
    if (rb) {
      const size = `${width}x${height}`;
      if (this.rbSizes.get(rb) !== size) {
        gl.bindRenderbuffer(GL.RENDERBUFFER, this.objs[rb]);
        gl.renderbufferStorage(GL.RENDERBUFFER, GL.DEPTH24_STENCIL8, width, height);
        this.rbSizes.set(rb, size);
      }
      gl.framebufferRenderbuffer(GL.DRAW_FRAMEBUFFER, GL.DEPTH_STENCIL_ATTACHMENT, GL.RENDERBUFFER, this.objs[rb]);
    }
    gl.bindFramebuffer(GL.DRAW_FRAMEBUFFER, prevFb);
    gl.bindRenderbuffer(GL.RENDERBUFFER, prevRb);
  }

  /** Draws texture `tex` on the canvas (texture row 0 at the bottom, like
   * upstream gfxstream's post) and hands the frame to the page. */
  #present(tex, width, height) {
    const gl = this.gl;
    const s = {
      program: gl.getParameter(gl.CURRENT_PROGRAM), vao: gl.getParameter(gl.VERTEX_ARRAY_BINDING),
      active: gl.getParameter(gl.ACTIVE_TEXTURE), draw: gl.getParameter(gl.DRAW_FRAMEBUFFER_BINDING),
      viewport: gl.getParameter(gl.VIEWPORT), mask: gl.getParameter(gl.COLOR_WRITEMASK),
    };
    gl.activeTexture(GL.TEXTURE0);
    const tex0 = gl.getParameter(gl.TEXTURE_BINDING_2D);
    const smp0 = gl.getParameter(gl.SAMPLER_BINDING);
    const caps = [GL.BLEND, GL.CULL_FACE, GL.DEPTH_TEST, GL.SCISSOR_TEST, GL.STENCIL_TEST, GL.RASTERIZER_DISCARD];
    const on = caps.map((c) => gl.isEnabled(c));
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
    gl.bindFramebuffer(GL.DRAW_FRAMEBUFFER, null);
    gl.viewport(0, 0, width, height);
    caps.forEach((c) => gl.disable(c));
    gl.colorMask(true, true, true, true);
    gl.useProgram(this.blit);
    gl.uniform1i(this.blitTex, 0);
    gl.bindVertexArray(this.blitVao);
    gl.bindTexture(GL.TEXTURE_2D, this.#obj(tex));
    gl.bindSampler(0, this.blitSampler);
    gl.drawArrays(GL.TRIANGLES, 0, 3);
    // Restore.
    gl.bindSampler(0, smp0);
    gl.bindTexture(GL.TEXTURE_2D, tex0);
    gl.activeTexture(s.active);
    gl.bindVertexArray(s.vao);
    gl.useProgram(s.program);
    caps.forEach((c, i) => (on[i] ? gl.enable(c) : gl.disable(c)));
    gl.colorMask(...s.mask);
    gl.viewport(...s.viewport);
    gl.bindFramebuffer(GL.DRAW_FRAMEBUFFER, s.draw);
    this.stats.frames++;
    const bitmap = this.canvas.transferToImageBitmap();
    this.onFrame?.(bitmap, width, height);
  }
}

/**
 * Replays a recording of `vetro boot --gpu=gfxstream --gl-record=FILE`
 * (crates/vetro-gfxstream/src/exec.rs, `Recorder`): per batch u32 words,
 * u32 blob bytes, u32 out bytes, then the three parts. Calls `onFrame` for
 * every presented frame.
 */
export function replayRecording(executor, bytes, { maxBatches = Infinity, onBatch = null } = {}) {
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let p = 0, batches = 0;
  while (p + 12 <= bytes.byteLength && batches < maxBatches) {
    const nw = dv.getUint32(p, true), nb = dv.getUint32(p + 4, true), no = dv.getUint32(p + 8, true);
    p += 12;
    const words = new Uint32Array(bytes.slice(p, p + nw * 4).buffer);
    p += nw * 4;
    const blob = bytes.slice(p, p + nb);
    p += nb;
    const out = new Uint8Array(no);
    p += no;
    executor.execute(words, blob, out);
    onBatch?.(out, batches);
    batches++;
  }
  return batches;
}
