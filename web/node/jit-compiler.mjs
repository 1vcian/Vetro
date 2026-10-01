// Background compilation of JIT modules (ADR 0038): a dedicated Worker
// (browser `Worker` or Node `worker_threads`) that compiles the modules the
// translator generates, so that the machine's thread does not.
//
// V8 compiles WebAssembly lazily: `new WebAssembly.Module` only decodes and
// validates, and each function is compiled by Liftoff the first time it is
// called, on the calling thread. The compiled code lives in the module
// (V8's NativeModule), which is shared by every thread the module is posted
// to. So the Worker compiles the module, calls every region once on a zeroed
// JitState in a scratch memory (limit 0: each region exits before its first
// instruction, having been compiled), and posts the module back; the
// machine's thread only instantiates it and finds its functions compiled.
//
// Protocol: in { id, gen, bytes } -> out { id, gen, module, ms } or
// { id, gen, error }. No state besides the scratch memory.

let scratch = new WebAssembly.Memory({ initial: 8 });

/** Compiles `bytes` and runs Liftoff on every exported region. */
function compile(bytes) {
  const module = new WebAssembly.Module(bytes);
  try {
    return warm(module);
  } catch (e) {
    // The threads build's modules import a shared memory (ADR 0042), which
    // only links to a shared scratch memory with the same maximum.
    if (!(e instanceof WebAssembly.LinkError) || scratch.buffer instanceof SharedArrayBuffer) throw e;
    scratch = new WebAssembly.Memory({ initial: 8, maximum: 65536, shared: true });
    return warm(module);
  }
}

function warm(module) {
  const imports = {};
  for (const imp of WebAssembly.Module.imports(module)) {
    let v;
    if (imp.kind === 'memory') v = scratch;
    else if (imp.kind === 'table') v = new WebAssembly.Table({ element: 'anyfunc', initial: 1 << 18 });
    else if (imp.kind === 'function') v = () => 0;
    else continue;
    (imports[imp.module] ??= {})[imp.name] = v;
  }
  const inst = new WebAssembly.Instance(module, imports);
  new Uint8Array(scratch.buffer).fill(0, 0, 4096);
  for (const [name, f] of Object.entries(inst.exports)) {
    if (!/^b\d+$/.test(name)) continue;
    try {
      f(0);
    } catch {
      // Compiled anyway: the call only has to reach the function.
    }
  }
  return module;
}

function onMessage({ id, gen, bytes }, post) {
  const t0 = performance.now();
  try {
    const module = compile(bytes);
    post({ id, gen, module, ms: performance.now() - t0 });
  } catch (e) {
    post({ id, gen, error: String(e) });
  }
}

if (typeof self !== 'undefined' && typeof self.postMessage === 'function' && typeof process === 'undefined') {
  self.onmessage = (e) => onMessage(e.data, (m) => self.postMessage(m));
} else {
  const { parentPort } = await import('node:worker_threads');
  parentPort.on('message', (m) => onMessage(m, (r) => parentPort.postMessage(r)));
}
