// The saver Worker (M6, ADR 0046), started by the machine's Worker: it turns
// the raw stream of a deferred save into the snapshot file in OPFS (our LZ, in
// its own vetro-wasm instance), off the machine's thread. Protocol in
// web/node/background-save.mjs.
//
// First message: `init` { module } (the compiled vetro-wasm module, shared by
// the machine's Worker), answered with `ready` or `error`.

import { instantiate } from '../node/vetro.mjs';
import { SnapshotStore } from '../node/persist.mjs';
import { SaveJob } from '../node/background-save.mjs';

let job = null;
let start = null;

onmessage = (e) => {
  const msg = e.data;
  if (msg.type === 'init') {
    start = (async () => {
      const { exports } = await instantiate(msg.module);
      job = new SaveJob(exports, await SnapshotStore.opfs(msg.dir), (reply, transfer = []) => postMessage(reply, transfer));
    })().then(
      () => postMessage({ type: 'ready' }),
      (err) => postMessage({ type: 'error', message: `saver: ${err.message ?? err}` }),
    );
    return;
  }
  start.then(() => job?.handle(msg));
};
