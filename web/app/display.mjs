// Disegno dello scanout di virtio-gpu sul canvas della pagina. Il Worker
// manda solo il rettangolo cambiato (RGBA, righe da `rect.width * 4` byte);
// qui lo si copia sul canvas:
//   - Canvas2D (default): putImageData del rettangolo;
//   - WebGPU (opzione): una texture rgba8unorm aggiornata con writeTexture
//     e disegnata con un triangolo che copre il canvas.

export class Canvas2DRenderer {
  name = 'canvas 2D';

  constructor(canvas) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false });
  }

  resize(width, height) {
    this.canvas.width = width;
    this.canvas.height = height;
  }

  draw(rect, pixels) {
    this.ctx.putImageData(new ImageData(pixels, rect.width, rect.height), rect.x, rect.y);
  }

  clear() {
    this.ctx.fillStyle = '#000';
    this.ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
  }
}

const SHADER = /* wgsl */ `
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
struct Out { @builtin(position) pos: vec4f, @location(0) uv: vec2f };
@vertex fn vs(@builtin(vertex_index) i: u32) -> Out {
  let p = array(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
  var o: Out;
  o.pos = vec4f(p[i], 0.0, 1.0);
  o.uv = vec2f((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
  return o;
}
@fragment fn fs(i: Out) -> @location(0) vec4f {
  return vec4f(textureSample(tex, smp, i.uv).rgb, 1.0);
}`;

export class WebGpuRenderer {
  name = 'WebGPU';

  /** Il renderer, o null se WebGPU non c'è. */
  static async create(canvas) {
    if (!navigator.gpu) return null;
    const adapter = await navigator.gpu.requestAdapter();
    if (!adapter) return null;
    const device = await adapter.requestDevice();
    return new WebGpuRenderer(canvas, device);
  }

  constructor(canvas, device) {
    this.canvas = canvas;
    this.device = device;
    this.ctx = canvas.getContext('webgpu');
    this.format = navigator.gpu.getPreferredCanvasFormat();
    this.ctx.configure({ device, format: this.format, alphaMode: 'opaque' });
    const module = device.createShaderModule({ code: SHADER });
    this.pipeline = device.createRenderPipeline({
      layout: 'auto',
      vertex: { module, entryPoint: 'vs' },
      fragment: { module, entryPoint: 'fs', targets: [{ format: this.format }] },
      primitive: { topology: 'triangle-list' },
    });
    this.sampler = device.createSampler({ magFilter: 'nearest', minFilter: 'linear' });
    this.texture = null;
  }

  resize(width, height) {
    this.canvas.width = width;
    this.canvas.height = height;
    this.texture?.destroy();
    this.texture = this.device.createTexture({
      size: [width, height],
      format: 'rgba8unorm',
      usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST,
    });
    this.bind = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: this.texture.createView() },
        { binding: 1, resource: this.sampler },
      ],
    });
  }

  draw(rect, pixels) {
    this.device.queue.writeTexture(
      { texture: this.texture, origin: [rect.x, rect.y] },
      pixels,
      { bytesPerRow: rect.width * 4, rowsPerImage: rect.height },
      [rect.width, rect.height],
    );
    this.#present();
  }

  #present() {
    const enc = this.device.createCommandEncoder();
    const pass = enc.beginRenderPass({
      colorAttachments: [{ view: this.ctx.getCurrentTexture().createView(), loadOp: 'clear', storeOp: 'store', clearValue: [0, 0, 0, 1] }],
    });
    if (this.bind) {
      pass.setPipeline(this.pipeline);
      pass.setBindGroup(0, this.bind);
      pass.draw(3);
    }
    pass.end();
    this.device.queue.submit([enc.finish()]);
  }

  clear() {
    this.bind = null;
    this.#present();
  }
}
