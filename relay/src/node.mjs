#!/usr/bin/env node
// The network gateway in Node (ADR 0046): the same protocol, policy, ledger
// and lane logic as the Cloudflare Worker, with `node:net` sockets and the
// `ws` WebSocket server, the ledger in memory. For the tests, the build VM
// and self-hosting.
//
//   TOKEN_SECRET=... ALLOWED_ORIGINS=http://127.0.0.1:8080 node relay/src/node.mjs [--port=8787] [--host=127.0.0.1]
//
// The same variables as the Worker (GATEWAY_ENABLED, ALLOWED_ORIGINS,
// TOKEN_SECRET, LANE_CONNS, LIMIT_*, DOH_URL), from the environment.

import { createServer } from 'node:http';
import { connect as netConnect } from 'node:net';
import { pathToFileURL } from 'node:url';
import { WebSocketServer } from 'ws';
import { checkLane, configFrom, handleHttp } from './gateway.mjs';
import { Lane } from './lane.mjs';
import { Ledger, limitsFrom } from './ledger.mjs';
import { DEFAULT_DOH, dohResolver } from './dns.mjs';

/** A `node:net` socket in Cloudflare's shape: { opened, readable, writable, close, reset }. */
export function nodeSocket({ hostname, port }) {
  const sock = netConnect({ host: hostname, port, allowHalfOpen: true });
  sock.setNoDelay(true);
  const opened = new Promise((ok, no) => {
    sock.once('connect', () => ok({}));
    sock.once('error', no);
  });
  opened.catch(() => {});
  const readable = new ReadableStream(
    {
      start(c) {
        sock.on('data', (d) => {
          c.enqueue(new Uint8Array(d));
          if (c.desiredSize <= 0) sock.pause();
        });
        sock.once('end', () => c.close());
        sock.once('error', (e) => c.error(e));
      },
      pull() {
        sock.resume();
      },
      cancel() {
        sock.destroy();
      },
    },
    new ByteLengthQueuingStrategy({ highWaterMark: 64 * 1024 }),
  );
  const writable = new WritableStream({
    write: (chunk) => new Promise((ok, no) => sock.write(chunk, (e) => (e ? no(e) : ok()))),
    close: () => new Promise((ok) => sock.end(ok)),
    abort: () => sock.destroy(),
  });
  return {
    opened,
    readable,
    writable,
    close: () => sock.destroy(),
    reset: () => sock.resetAndDestroy(),
  };
}

/**
 * Starts the gateway. Options override the environment variables:
 * { port, host, vars, connect, resolver, log, laneOptions }. Resolves to
 * { url, port, close(), sessions }.
 */
export async function startGateway({ port = 8787, host = '127.0.0.1', vars = process.env, connect = nodeSocket, resolver = null, log, laneOptions = {} } = {}) {
  const cfg = configFrom(vars);
  if (!cfg.secret) throw new Error('TOKEN_SECRET is required');
  const limits = limitsFrom(vars);
  const dns = resolver ?? dohResolver(vars.DOH_URL || DEFAULT_DOH);
  const sessions = new Map();
  const ledgerOf = (sid) => {
    let l = sessions.get(sid);
    if (!l) {
      l = new Ledger(limits, { enabled: () => cfg.enabled });
      sessions.set(sid, l);
    }
    return l;
  };
  const toRequest = (req, body) =>
    new Request(`http://${req.headers.host ?? 'localhost'}${req.url}`, {
      method: req.method,
      headers: Object.entries(req.headers).filter(([, v]) => typeof v === 'string'),
      body: body && req.method !== 'GET' && req.method !== 'HEAD' ? body : undefined,
    });

  const server = createServer(async (req, res) => {
    const chunks = [];
    let size = 0;
    for await (const c of req) {
      size += c.length;
      if (size > 4096) return res.writeHead(413).end();
      chunks.push(c);
    }
    const r = await handleHttp(toRequest(req, Buffer.concat(chunks)), cfg, { lanes: limits.lanes });
    res.writeHead(r.status, Object.fromEntries(r.headers));
    res.end(Buffer.from(await r.arrayBuffer()));
  });
  const wss = new WebSocketServer({ noServer: true, maxPayload: 64 * 1024 });
  server.on('upgrade', async (req, sock, head) => {
    const refuse = (status, why) => {
      sock.end(`HTTP/1.1 ${status} ${why}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n`);
    };
    const lane = await checkLane(toRequest(req), cfg, { lanes: limits.lanes });
    if (lane.error) return refuse(lane.status, lane.error);
    const ledger = ledgerOf(lane.sid);
    const open = ledger.openLane(lane.lane);
    if (!open.ok) return refuse(open.reason === 'disabled' ? 503 : open.reason === 'lane-in-use' ? 409 : 429, open.reason);
    wss.handleUpgrade(req, sock, head, (ws) => {
      const l = new Lane({
        send: (d) => ws.send(d),
        close: (code, reason) => ws.close(code, reason),
        connect,
        resolver: dns,
        ledger,
        lane: lane.lane,
        laneConns: cfg.laneConns,
        limits,
        platform: 'node',
        ...(log ? { log } : {}),
        ...laneOptions,
      });
      ws.on('message', (data, binary) => l.onMessage(binary ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : data.toString()));
      ws.on('close', () => l.onClose());
      ws.on('error', () => l.onClose());
      l.start();
    });
  });
  await new Promise((ok) => server.listen(port, host, ok));
  const actual = server.address().port;
  return {
    url: `http://${host}:${actual}`,
    port: actual,
    sessions,
    cfg,
    close: () =>
      new Promise((ok) => {
        for (const ws of wss.clients) ws.terminate();
        server.close(() => ok());
        server.closeAllConnections?.();
      }),
  };
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  const arg = (k, d) => process.argv.find((a) => a.startsWith(`--${k}=`))?.slice(k.length + 3) ?? d;
  const g = await startGateway({ port: Number(arg('port', 8787)), host: arg('host', '127.0.0.1') });
  console.error(`vetro gateway: ${g.url} (origins: ${g.cfg.origins.join(', ') || 'none'}, enabled: ${g.cfg.enabled})`);
}
