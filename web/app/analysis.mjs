// I pannelli di analisi della pagina (M7, M10, ADR 0023): ispettore di rete,
// timeline input→effetti e registrazione/replay. I dati arrivano dal Worker
// (messaggi `analysis`, `rr`, `paused`, `replay-started`, `replay-ended`);
// dettaglio, esportazioni, registri e memoria si chiedono con `call(op,
// args)` (messaggio `inspect`), i comandi di registrazione con `command(op,
// args)` (messaggio `rr`).
//
// Esportazioni: Blob + <a download>, che nell'app (anche su GitHub Pages)
// scarica il file. Lo stato è leggibile da `window.vetroAnalysis` (test).

const $ = (id) => document.getElementById(id);

/** Tempo del guest (µs) come secondi con i millisecondi. */
export function guestTime(us) {
  return `${Math.floor(us / 1e6)}.${String(Math.floor(us / 1000) % 1000).padStart(3, '0')} s`;
}

/** Durata in µs, leggibile. */
export function duration(us) {
  if (us === null || us === undefined) return '–';
  if (us < 1000) return `${us} µs`;
  if (us < 1e6) return `${(us / 1000).toFixed(us < 1e4 ? 2 : 1)} ms`;
  return `${(us / 1e6).toFixed(2)} s`;
}

export function bytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 2 ** 20).toFixed(1)} MiB`;
}

/**
 * Cella del corpo nella lista: "0 B" per un corpo vuoto (Content-Length: 0
 * o nessun byte), dimensione e tipo riconosciuto altrimenti; "–" solo se
 * l'informazione manca (dimensione assente).
 */
export function bodyCell(size, kind) {
  if (size === null || size === undefined) return '–';
  if (!size) return '0 B';
  return kind && kind !== '-' ? `${bytes(size)} ${kind}` : bytes(size);
}

/**
 * Il tipo della risposta: il Content-Type, se c'è; altrimenti quello
 * riconosciuto dal contenuto (attenuato); "–" senza risposta.
 */
export function typeText(r) {
  if (r.mime) return { text: r.mime, title: r.mime, guessed: false };
  if (r.status === null || r.status === undefined) return { text: '–', title: 'senza risposta', guessed: false };
  const kind = r.respBytes ? r.respKind : 'vuoto';
  return { text: kind, title: 'nessun Content-Type: tipo riconosciuto dal contenuto', guessed: true };
}

function typeCell(r) {
  const t = typeText(r);
  return el('td', { text: t.text, title: t.title, class: t.guessed ? 'guessed' : '' });
}

/** Byte da base64. */
export function fromB64(s) {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Dump esadecimale (16 byte per riga) a partire dall'indirizzo `base` (BigInt). */
export function hexdump(data, base = 0n, max = 64 * 1024) {
  const rows = [];
  const n = Math.min(data.length, max);
  for (let o = 0; o < n; o += 16) {
    const chunk = data.subarray(o, Math.min(o + 16, n));
    const hex = [...chunk].map((b) => b.toString(16).padStart(2, '0')).join(' ');
    const ascii = [...chunk].map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.')).join('');
    rows.push(`${(base + BigInt(o)).toString(16).padStart(16, '0')}  ${hex.padEnd(47)}  ${ascii}`);
  }
  if (data.length > n) rows.push(`… altri ${data.length - n} byte`);
  return rows.join('\n');
}

/** Scarica `data` (Uint8Array o stringa) come file. */
export function download(data, name, type = 'application/octet-stream') {
  const url = URL.createObjectURL(new Blob([data], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.hidden = true;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') e.className = v;
    else if (k === 'text') e.textContent = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v !== null && v !== undefined && v !== false) e.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) if (c !== null && c !== undefined) e.append(c);
  return e;
}

const stamp = () => new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);

// ---- Ispettore di rete --------------------------------------------------------

class NetPanel {
  constructor(app) {
    this.app = app;
    this.data = { frames: 0, requests: [], dns: [], tls: [] };
    this.capture = null;
    this.selected = null;
    this.detail = null;
    this.body = $('net-table').tBodies[0];
    for (const id of ['net-filter', 'net-method', 'net-status', 'net-kind']) $(id).addEventListener('input', () => this.render());
    $('net-har').addEventListener('click', () => this.export('har'));
    $('net-pcap').addEventListener('click', () => this.export('pcapng'));
    $('net-clear').addEventListener('click', () => app.call('capture-clear'));
    this.body.addEventListener('click', (e) => {
      const tr = e.target.closest('tr[data-i]');
      if (tr) this.select(Number(tr.dataset.i));
    });
  }

  update(requests, capture) {
    this.data = requests;
    this.capture = capture;
    const methods = [...new Set(requests.requests.map((r) => r.method))].sort();
    const sel = $('net-method');
    const cur = sel.value;
    sel.replaceChildren(el('option', { value: '', text: 'tutti i metodi' }), ...methods.map((m) => el('option', { value: m, text: m })));
    sel.value = methods.includes(cur) ? cur : '';
    $('tab-count-net').textContent = requests.requests.length ? `(${requests.requests.length})` : '';
    this.render();
    if (this.selected !== null && this.selected >= requests.requests.length) {
      this.selected = null;
      this.detail = null;
      $('net-detail').replaceChildren();
    }
  }

  visible() {
    const q = $('net-filter').value.trim().toLowerCase();
    const method = $('net-method').value;
    const st = $('net-status').value;
    const kind = $('net-kind').value;
    return this.data.requests.filter((r) => {
      if (q && !`${r.method} ${r.host} ${r.path} ${r.status ?? ''} ${r.mime ?? ''}`.toLowerCase().includes(q)) return false;
      if (method && r.method !== method) return false;
      if (st === 'none' ? r.status !== null : st && String(r.status ?? '')[0] !== st) return false;
      if (kind && r.reqKind !== kind && r.respKind !== kind) return false;
      return true;
    });
  }

  render() {
    const rows = this.visible();
    const t0 = rows.length ? Math.min(...rows.map((r) => r.timings.startedUs)) : 0;
    const t1 = rows.length ? Math.max(...rows.map((r) => r.timings.startedUs + r.timings.totalUs)) : 1;
    const span = Math.max(1, t1 - t0);
    this.body.replaceChildren(...rows.map((r) => {
      const left = ((r.timings.startedUs - t0) / span) * 100;
      const width = Math.max(0.5, (r.timings.totalUs / span) * 100);
      return el('tr', { 'data-i': r.i, class: r.i === this.selected ? 'sel' : '' },
        el('td', { text: r.i }),
        el('td', { text: guestTime(r.timings.startedUs) }),
        el('td', { class: 'method', text: r.method }),
        el('td', { text: r.host, title: r.host }),
        el('td', { class: 'path', text: r.path, title: r.url }),
        el('td', { class: `status s${String(r.status ?? 0)[0]}`, text: r.status ?? '—', title: r.reason ?? 'senza risposta' }),
        el('td', { class: 'num', text: bodyCell(r.reqBytes, r.reqKind) }),
        el('td', { class: 'num', text: r.status === null ? '–' : bodyCell(r.respBytes, r.respKind), title: r.status === null ? 'senza risposta' : '' }),
        typeCell(r),
        el('td', { class: 'num', text: duration(r.timings.totalUs) }),
        el('td', { class: 'wf' }, el('span', { style: `left:${left}%;width:${width}%` })),
      );
    }));
    const c = this.capture;
    $('net-info').textContent = c
      ? `${rows.length}/${this.data.requests.length} richieste · ${this.data.dns.length} DNS · ${this.data.tls.length} TLS · ${c.frames} frame (${bytes(c.bytes)})${c.dropped ? ` · ${c.dropped} scartati` : ''}${c.on ? '' : ' · cattura spenta'}`
      : '';
  }

  async select(i) {
    this.selected = i;
    this.render();
    const d = await this.app.call('request', { index: i });
    if (this.selected !== i) return;
    this.detail = d;
    $('net-detail').replaceChildren(...(d ? this.renderDetail(d) : [el('p', { text: 'richiesta non trovata' })]));
  }

  renderDetail(d) {
    const r = d.row;
    const t = r.timings;
    const phases = [['blocked', t.blockedUs], ['dns', t.dnsUs], ['connect', t.connectUs], ['send', t.sendUs], ['wait', t.waitUs], ['receive', t.receiveUs]];
    const total = Math.max(1, t.totalUs);
    const general = el('section', { class: 'nd-general' },
      el('div', { class: 'nd-url' }, el('strong', { text: `${r.method} ` }), r.url),
      el('div', { text: `stato: ${r.status ?? 'senza risposta'} ${r.reason ?? ''} · ${r.client} → ${r.server}${r.resolvedName ? ` (${r.resolvedName})` : ''} · flusso ${r.flow}` }),
      el('div', { text: `inizio ${guestTime(t.startedUs)} · totale ${duration(t.totalUs)}` }),
      el('div', { class: 'phases' }, ...phases.filter(([, v]) => v !== null).map(([n, v]) =>
        el('span', { class: `ph ph-${n}`, style: `flex-grow:${Math.max(v / total, 0.02)}`, title: `${n}: ${duration(v)}`, text: `${n} ${duration(v)}` }))),
    );
    const sections = [general, this.message('Richiesta', `${d.request.method} ${d.request.target} ${d.request.version}`, d.request)];
    if (d.response) sections.push(this.message('Risposta', `${d.response.version} ${d.response.status} ${d.response.reason}`, d.response));
    else sections.push(el('section', {}, el('h4', { text: 'Risposta' }), el('p', { class: 'dim', text: 'nessuna risposta nella cattura' })));
    return sections;
  }

  message(title, line, msg) {
    const headers = el('table', { class: 'kv' }, ...msg.headers.map(([k, v]) => el('tr', {}, el('th', { text: k }), el('td', { text: v }))));
    return el('section', {},
      el('h4', { text: title }),
      el('div', { class: 'mono', text: line }),
      el('details', { open: true }, el('summary', { text: `intestazioni (${msg.headers.length})` }), headers),
      this.body_(msg.body),
    );
  }

  body_(b) {
    const info = [`${bytes(b.size)}`, b.kind];
    if (b.encoding) info.push(`${b.encoding} (${bytes(b.raw)} sul filo)`);
    if (b.chunked) info.push('chunked');
    if (b.decodeError) info.push(`non decodificato: ${b.decodeError}`);
    const view = el('div', { class: 'body-view' });
    const mode = el('select', { class: 'body-mode', title: 'Visualizzazione del corpo' },
      el('option', { value: 'decoded', text: 'decodificato' }), el('option', { value: 'hex', text: 'esadecimale' }));
    const show = () => view.replaceChildren(mode.value === 'hex' ? this.hex(b) : this.decoded(b));
    mode.addEventListener('input', show);
    show();
    return el('details', { open: b.size > 0 },
      el('summary', {}, `corpo: ${info.join(' · ')} `, b.size ? mode : null),
      view);
  }

  hex(b) {
    const data = fromB64(b.base64);
    return el('pre', { class: 'hex', text: hexdump(data) + (b.truncated ? `\n… (corpo di ${bytes(b.size)}, mostrati i primi ${bytes(data.length)})` : '') });
  }

  decoded(b) {
    switch (b.kind) {
      case 'vuoto':
        return el('p', { class: 'dim', text: '(nessun corpo)' });
      case 'json':
        return el('pre', { class: 'json', text: b.text });
      case 'form':
        return el('table', { class: 'kv' }, ...b.fields.map(([k, v]) => el('tr', {}, el('th', { text: k }), el('td', { text: v }))));
      case 'multipart':
        return el('div', {}, ...b.parts.map((p, i) => el('div', { class: 'part' },
          el('div', { class: 'mono', text: `parte ${i + 1}: name=${p.name ?? '-'} filename=${p.filename ?? '-'} type=${p.contentType ?? '-'} (${bytes(p.size)}, ${p.kind})` }),
          p.kind === 'form' ? this.decoded(p) : el('pre', { text: p.text }))));
      case 'binario':
        return el('div', {}, el('p', { class: 'dim', text: b.note ? `non decodificato: ${b.note}` : 'byte non riconosciuti' }), this.hex(b));
      default:
        return el('pre', { class: b.kind, text: b.text + (b.textTruncated ? '\n… (tagliato)' : '') });
    }
  }

  async export(what) {
    try {
      const data = await this.app.call(what);
      const name = `vetro-${stamp()}.${what === 'har' ? 'har' : 'pcapng'}`;
      download(data, name, what === 'har' ? 'application/json' : 'application/vnd.tcpdump.pcap');
      this.app.state.downloads.push({ what, name, size: data.length });
    } catch (e) {
      this.app.setStatus(`esportazione non riuscita: ${e.message}`);
    }
  }
}

// ---- Timeline ------------------------------------------------------------------

const INPUT_COLORS = { tasto: '#6cb6ff', puntatore: '#d2a8ff', tocco: '#d2a8ff', console: '#7ee787', file: '#ffa657', accensione: '#ff7b72', schermo: '#8a96a3', altro: '#8a96a3' };
const EFFECT_COLORS = { http: '#79c0ff', dns: '#56d4dd', tls: '#e3b341', file: '#ffa657', console: '#8b949e' };

class TimelinePanel {
  constructor(app) {
    this.app = app;
    this.data = { inputs: [], effects: [], windowUs: 3e6 };
    for (const id of ['tl-http', 'tl-dns', 'tl-tls', 'tl-file', 'tl-console', 'tl-weak']) $(id).addEventListener('input', () => this.render());
    $('tl-window').addEventListener('change', () => app.call('window', { us: Math.max(1, Number($('tl-window').value)) * 1000 }));
    $('tl-clear').addEventListener('click', () => app.call('timeline-clear'));
    $('tl-list').addEventListener('click', (e) => {
      const go = e.target.closest('[data-goto]');
      if (go) return app.replay.gotoStep(Number(go.dataset.goto));
      const req = e.target.closest('[data-req]');
      if (req) {
        app.showTab('net');
        app.net.select(Number(req.dataset.req));
      }
    });
  }

  update(t) {
    this.data = t;
    this.render();
  }

  shown(kind) {
    return $(`tl-${kind}`)?.checked ?? true;
  }

  render() {
    const t = this.data;
    const weak = $('tl-weak').checked;
    const showInput = (i) => weak || !i.weak;
    const byCause = new Map();
    const loose = [];
    for (const e of t.effects) {
      if (!this.shown(e.kind)) continue;
      if (e.cause !== null && showInput(t.inputs[e.cause])) {
        if (!byCause.has(e.cause)) byCause.set(e.cause, []);
        byCause.get(e.cause).push(e);
      } else if (e.cause === null || e.kind !== 'console') loose.push(e);
    }
    // Ingressi (con i loro effetti) ed effetti senza causa, in ordine di tempo.
    const items = [
      ...t.inputs.filter(showInput).map((i) => ({ at: i.atUs, input: i })),
      ...loose.map((e) => ({ at: e.atUs, effect: e })),
    ].sort((a, b) => a.at - b.at || (a.input ? -1 : 1));
    const rr = this.app.replay.info;
    const canGo = (step) => rr && rr.sameMachine && step >= rr.startSteps && step <= rr.endSteps;
    const list = items.slice(-2000).map((it) => {
      if (it.effect) return this.effectRow(it.effect, false);
      const i = it.input;
      const effects = byCause.get(i.i) ?? [];
      return el('div', { class: 'tl-input', 'data-input': i.i },
        el('div', { class: 'tl-head' },
          el('span', { class: 'tl-time', text: guestTime(i.atUs) }),
          el('span', { class: 'badge', style: `background:${INPUT_COLORS[i.kind] ?? '#888'}`, text: i.kind }),
          el('span', { class: 'tl-label', text: i.label }),
          el('span', { class: 'dim', text: `istruzione ${i.step}${effects.length ? ` · ${effects.length} effetti` : ''}` }),
          canGo(i.step) ? el('button', { class: 'fsmall', 'data-goto': i.step, title: 'Replay fino a questa istruzione: registri e memoria di quel momento', text: 'vai qui' }) : null),
        effects.length ? el('div', { class: 'tl-effects' }, ...effects.slice(0, 50).map((e) => this.effectRow(e, true)),
          effects.length > 50 ? el('div', { class: 'dim', text: `… altri ${effects.length - 50}` }) : null) : null);
    });
    $('tl-list').replaceChildren(...(list.length ? list : [el('p', { class: 'dim', text: 'nessun ingresso né effetto finora' })]));
    $('tl-info').textContent = `${t.inputs.length} ingressi · ${t.effects.length} effetti · finestra ${duration(t.windowUs)}${t.dropped ? ` · ${t.dropped} scartati` : ''}`;
    this.track(t, showInput);
  }

  effectRow(e, nested) {
    const delay = nested && e.cause !== null ? ` +${duration(e.atUs - this.data.inputs[e.cause].atUs)}` : '';
    return el('div', { class: `tl-effect${nested ? '' : ' loose'}`, 'data-kind': e.kind },
      el('span', { class: 'tl-time', text: nested ? delay : guestTime(e.atUs) }),
      el('span', { class: 'badge', style: `background:${EFFECT_COLORS[e.kind] ?? '#888'}`, text: e.kind }),
      e.ref !== null ? el('a', { href: '#', 'data-req': e.ref, class: 'tl-label', text: e.label }) : el('span', { class: 'tl-label', text: e.label }),
      nested ? null : el('span', { class: 'dim', text: 'senza causa' }));
  }

  /** Asse del tempo del guest: ingressi in alto, effetti per tipo sotto. */
  track(t, showInput) {
    const svg = $('tl-track');
    const w = svg.clientWidth || 800;
    const lanes = ['http', 'dns', 'tls', 'file', 'console'];
    const h = 22 + lanes.length * 10;
    svg.setAttribute('viewBox', `0 0 ${w} ${h}`);
    svg.setAttribute('height', h);
    const times = [...t.inputs.map((i) => i.atUs), ...t.effects.map((e) => e.atUs)];
    if (!times.length) return svg.replaceChildren();
    const t0 = Math.min(...times);
    const span = Math.max(1, Math.max(...times) - t0);
    const x = (us) => 4 + ((us - t0) / span) * (w - 8);
    const ns = 'http://www.w3.org/2000/svg';
    const mk = (tag, a) => {
      const e = document.createElementNS(ns, tag);
      for (const [k, v] of Object.entries(a)) e.setAttribute(k, v);
      return e;
    };
    const out = [mk('line', { x1: 0, x2: w, y1: 16, y2: 16, stroke: '#2c333b' })];
    for (const i of t.inputs) {
      if (!showInput(i)) continue;
      const l = mk('line', { x1: x(i.atUs), x2: x(i.atUs), y1: 2, y2: 16, stroke: INPUT_COLORS[i.kind] ?? '#888', 'stroke-width': i.weak ? 1 : 2 });
      l.append(Object.assign(document.createElementNS(ns, 'title'), { textContent: `${guestTime(i.atUs)} ${i.label}` }));
      out.push(l);
    }
    for (const e of t.effects) {
      if (!this.shown(e.kind)) continue;
      const c = mk('circle', { cx: x(e.atUs), cy: 22 + lanes.indexOf(e.kind) * 10, r: 3, fill: EFFECT_COLORS[e.kind] ?? '#888' });
      c.append(Object.assign(document.createElementNS(ns, 'title'), { textContent: `${guestTime(e.atUs)} ${e.label}` }));
      out.push(c);
    }
    const label = (tx, anchor, txt) => {
      const e = mk('text', { x: tx, y: 12, fill: '#8a96a3', 'font-size': 10, 'text-anchor': anchor });
      e.textContent = txt;
      return e;
    };
    out.push(label(w - 4, 'end', guestTime(t0 + span)), label(4, 'start', guestTime(t0)));
    svg.replaceChildren(...out);
  }
}

// ---- Registrazione e replay -----------------------------------------------------

const RR_TEXT = { Idle: 'fermo', Recording: 'registrazione in corso', Replaying: 'replay in corso', Finished: 'replay identico', Diverged: 'replay diverso' };

class ReplayPanel {
  constructor(app) {
    this.app = app;
    this.status = null;
    this.info = null;
    this.mode = 'live';
    this.verdict = null;
    this.paused = null;
    this.memory = null;
    $('rr-record').addEventListener('click', () => {
      if (this.status?.state === 'Recording') app.command('record-stop');
      else app.command('record-start', { keyframeEvery: Math.max(1, Number($('rr-kf').value)) * 1e6 });
    });
    $('rr-replay').addEventListener('click', () => {
      this.verdict = null;
      app.command('replay', { step: 0 });
    });
    $('rr-download').addEventListener('click', () => this.download());
    $('rr-load').addEventListener('change', async (e) => {
      const f = e.target.files[0];
      if (!f) return;
      const buf = await f.arrayBuffer();
      e.target.value = '';
      app.command('load-log', { bytes: buf }, [buf]);
    });
    $('rr-goto').addEventListener('click', () => this.gotoStep(Number($('rr-step').value)));
    $('rr-continue').addEventListener('click', () => app.command('continue'));
    $('rr-read').addEventListener('click', () => this.readMemory());
    $('rr-events').addEventListener('click', (e) => {
      const go = e.target.closest('[data-goto]');
      if (go) this.gotoStep(Number(go.dataset.goto));
    });
  }

  gotoStep(step) {
    if (!Number.isFinite(step)) return;
    $('rr-step').value = String(step);
    this.app.showTab('replay');
    this.verdict = null;
    this.paused = null;
    this.app.command('replay', { step, pause: true });
  }

  async download() {
    try {
      const data = await this.app.call('log');
      if (!data.length) throw new Error('nessuna registrazione');
      const name = `vetro-${stamp()}.vrec`;
      download(data, name);
      this.app.state.downloads.push({ what: 'log', name, size: data.length });
    } catch (e) {
      this.app.setStatus(`log non scaricato: ${e.message}`);
    }
  }

  async readMemory() {
    const va = $('rr-va').value.trim();
    const length = Math.min(Math.max(1, Number($('rr-len').value) || 256), 64 * 1024);
    try {
      const r = await this.app.call('memory', { va: BigInt(va).toString(), length });
      this.memory = r;
      $('rr-mem').textContent = r.bytes
        ? `${va} → fisico ${r.pa ? `0x${r.pa}` : '?'}\n${hexdump(r.bytes, BigInt(va))}`
        : `indirizzo non leggibile: ${r.fault} (non mappato o fuori dalla RAM)`;
    } catch (e) {
      $('rr-mem').textContent = `indirizzo non valido: ${e.message}`;
    }
  }

  async refreshEvents() {
    const events = this.info ? await this.app.call('events') : [];
    // Solo i comandi (Invio, clic, tocchi, ...): i singoli caratteri no.
    const user = events.filter((e) => e.user && !e.weak);
    $('rr-events').replaceChildren(...user.slice(-500).map((e) => el('div', { class: 'tl-head' },
      el('span', { class: 'tl-time', text: `istr. ${e.step}` }),
      el('span', { class: 'badge', style: `background:${INPUT_COLORS[e.kind] ?? '#888'}`, text: e.kind }),
      el('span', { class: 'tl-label', text: e.label }),
      this.info?.sameMachine ? el('button', { class: 'fsmall', 'data-goto': e.step, text: 'vai qui' }) : null)));
    $('rr-events-count').textContent = `${user.length} comandi dell'utente su ${events.length} eventi`;
  }

  update(msg) {
    const hadLog = this.info ? `${this.info.startSteps}/${this.info.endSteps}/${this.info.events}` : '';
    this.status = msg.status;
    this.info = msg.info;
    this.mode = msg.mode;
    const s = msg.status;
    const rec = s.state === 'Recording';
    $('rr-record').textContent = rec ? 'Ferma' : 'Registra';
    $('rr-record').classList.toggle('recording', rec);
    const live = msg.mode === 'live';
    $('rr-replay').disabled = !this.info?.sameMachine || rec;
    $('rr-download').disabled = !this.info || rec;
    $('rr-goto').disabled = !this.info?.sameMachine || rec;
    $('rr-continue').disabled = msg.mode !== 'paused';
    const parts = [];
    if (rec) parts.push(`registrazione: ${s.progress} ingressi`);
    else if (msg.mode === 'replay') parts.push(`replay: evento ${s.progress} di ${s.events}${msg.target !== null ? `, fino all'istruzione ${msg.target}` : ''}`);
    else if (msg.mode === 'paused') parts.push(`fermo all'istruzione ${msg.steps}`);
    if (this.verdict) parts.push(this.verdict.text);
    if (this.info) {
      parts.push(`log: istruzioni ${this.info.startSteps}–${this.info.endSteps}, ${this.info.events} eventi, ${this.info.keyframes} keyframe` +
        `${msg.meta ? ` in OPFS (${bytes(msg.meta.sizes.reduce((a, b) => a + b, 0))})` : ''}${this.info.sameMachine ? '' : ', di un\'altra macchina'}`);
    } else if (!rec) parts.push('nessuna registrazione');
    $('rr-status').textContent = parts.join(' · ');
    $('rr-status').className = this.verdict?.ok === false ? 'err' : '';
    const now = this.info ? `${this.info.startSteps}/${this.info.endSteps}/${this.info.events}` : '';
    if (now !== hadLog && live) this.refreshEvents();
    this.app.state.rr = { ...msg, verdict: this.verdict, paused: this.paused };
    if (this.app.timeline) this.app.timeline.render();
  }

  onPaused(msg) {
    this.paused = msg;
    $('rr-regs').textContent = msg.registers;
    const vbar = /vbar_el1\s+([0-9a-f]+)/.exec(msg.registers);
    if (vbar && !$('rr-va').value) $('rr-va').value = `0x${vbar[1]}`;
    this.app.state.rr = { ...this.app.state.rr, paused: msg };
    if ($('rr-va').value) this.readMemory();
  }

  onEnded(msg) {
    const ok = msg.status.state === 'Finished';
    this.verdict = ok
      ? { ok, text: `replay identico: stesso stato all'istruzione ${msg.steps}` }
      : { ok, text: `replay diverso: ${msg.status.message}` };
    this.app.state.rr = { ...this.app.state.rr, verdict: this.verdict };
  }
}

// ---- Insieme -----------------------------------------------------------------------

export class AnalysisPanels {
  /**
   * `post(msg, transfer)`: manda al Worker; `setStatus(testo)`: la barra
   * di stato della pagina.
   */
  constructor({ post, setStatus }) {
    this.post = post;
    this.setStatus = setStatus;
    this.pending = new Map();
    this.id = 0;
    this.state = { requests: null, timeline: null, capture: null, rr: null, downloads: [], replayStarted: null, replayEnded: null };
    this.net = new NetPanel(this);
    this.replay = new ReplayPanel(this);
    this.timeline = new TimelinePanel(this);
    for (const b of document.querySelectorAll('#tabs [data-tab]')) b.addEventListener('click', () => this.showTab(b.dataset.tab));
    window.vetroAnalysis = {
      state: () => this.state,
      detail: () => this.net.detail,
      select: (i) => this.net.select(i),
    };
  }

  showTab(name) {
    for (const b of document.querySelectorAll('#tabs [data-tab]')) b.setAttribute('aria-selected', String(b.dataset.tab === name));
    for (const t of document.querySelectorAll('#analysis-box .tab')) t.hidden = t.id !== `tab-${name}`;
    if (name === 'timeline') this.timeline.render();
  }

  /** Una lettura nel Worker: Promise del risultato. */
  call(op, args = {}) {
    return new Promise((ok, ko) => {
      const id = ++this.id;
      this.pending.set(id, { ok, ko });
      this.post({ type: 'inspect', id, op, ...args });
    });
  }

  command(op, args = {}, transfer = []) {
    this.post({ type: 'rr', op, ...args }, transfer);
  }

  /** Un messaggio del Worker; true se era per questi pannelli. */
  onMessage(msg) {
    switch (msg.type) {
      case 'analysis':
        this.state.requests = msg.requests;
        this.state.timeline = msg.timeline;
        this.state.capture = msg.capture;
        this.net.update(msg.requests, msg.capture);
        this.timeline.update(msg.timeline);
        return true;
      case 'inspect-reply': {
        const p = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.ok) p?.ok(msg.result);
        else p?.ko(new Error(msg.error));
        return true;
      }
      case 'rr':
        this.replay.update(msg);
        return true;
      case 'paused':
        this.replay.onPaused(msg);
        return true;
      case 'replay-started':
        this.state.replayStarted = msg;
        return true;
      case 'replay-ended':
        this.state.replayEnded = msg;
        this.replay.onEnded(msg);
        return true;
      case 'rr-error':
        this.replay.verdict = { ok: false, text: `${msg.op}: ${msg.message}` };
        return true;
      default:
        return false;
    }
  }
}
