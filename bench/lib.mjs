// Shared plumbing for the kite vs chrome-devtools-mcp benchmark.
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, existsSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir, homedir } from 'node:os';
import { join } from 'node:path';

export const KITE = process.env.KITE_BIN || 'kite';
export const now = () => performance.now();

// ------------------------------------------------------------------ chrome
export function chromePath() {
  if (process.env.BENCH_CHROME) return process.env.BENCH_CHROME;
  const pw = join(homedir(), process.platform === 'darwin' ? 'Library/Caches/ms-playwright' : '.cache/ms-playwright');
  if (existsSync(pw)) {
    const shells = readdirSync(pw).filter((d) => d.startsWith('chromium_headless_shell-')).sort((a, b) => +b.split('-')[1] - +a.split('-')[1]);
    for (const d of shells) {
      for (const c of ['chrome-mac/headless_shell', 'chrome-headless-shell-mac-arm64/chrome-headless-shell', 'chrome-linux/headless_shell', 'chrome-headless-shell-linux64/chrome-headless-shell']) {
        if (existsSync(join(pw, d, c))) return join(pw, d, c);
      }
    }
  }
  return '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
}

/** Launch a headless Chrome with a debugging port; returns {browserUrl, wsUrl, kill}. */
export async function launchChrome() {
  const dir = mkdtempSync(join(tmpdir(), 'bench-chrome-'));
  const bin = chromePath();
  const args = [`--user-data-dir=${dir}`, '--remote-debugging-port=0', '--no-first-run', '--no-default-browser-check',
    '--disable-background-networking', '--disable-sync', '--mute-audio', '--hide-scrollbars', '--window-size=1280,800', 'about:blank'];
  if (!bin.includes('headless_shell') && !bin.includes('chrome-headless-shell')) args.unshift('--headless=new');
  const proc = spawn(bin, args, { stdio: 'ignore' });
  const portFile = join(dir, 'DevToolsActivePort');
  const t0 = now();
  while (!existsSync(portFile) || readFileSync(portFile, 'utf8').split('\n').length < 2) {
    if (now() - t0 > 15000) throw new Error('chrome did not start');
    await new Promise((r) => setTimeout(r, 20));
  }
  const [port, path] = readFileSync(portFile, 'utf8').trim().split('\n');
  return {
    browserUrl: `http://127.0.0.1:${port}`,
    wsUrl: `ws://127.0.0.1:${port}${path}`,
    kill() { proc.kill('SIGKILL'); try { rmSync(dir, { recursive: true, force: true }); } catch {} },
  };
}

/** One-shot CDP call over a websocket (used to seed cookies before MCP attaches). */
export async function cdp(wsUrl, calls) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id && pending.has(d.id)) { pending.get(d.id)(d); pending.delete(d.id); } };
  const out = [];
  for (const [method, params] of calls) {
    const my = ++id;
    out.push(await new Promise((r) => { pending.set(my, r); ws.send(JSON.stringify({ id: my, method, params })); }));
  }
  ws.close();
  return out;
}

// ------------------------------------------------------------------ MCP stdio client
export class Mcp {
  constructor(cmd, args) {
    this.proc = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    this.buf = '';
    this.id = 0;
    this.pending = new Map();
    this.stderr = '';
    this.proc.stderr.on('data', (d) => { this.stderr += d; });
    this.proc.stdout.on('data', (d) => {
      this.buf += d;
      let i;
      while ((i = this.buf.indexOf('\n')) >= 0) {
        const line = this.buf.slice(0, i); this.buf = this.buf.slice(i + 1);
        if (!line.trim()) continue;
        let msg; try { msg = JSON.parse(line); } catch { continue; }
        if (msg.id != null && this.pending.has(msg.id)) { this.pending.get(msg.id)(msg); this.pending.delete(msg.id); }
      }
    });
  }
  request(method, params, timeoutMs = 60000) {
    const id = ++this.id;
    return new Promise((res, rej) => {
      const t = setTimeout(() => rej(new Error(`${method} timed out; stderr: ${this.stderr.slice(-500)}`)), timeoutMs);
      this.pending.set(id, (m) => { clearTimeout(t); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); });
      this.proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }
  notify(method, params) { this.proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n'); }
  async init() {
    await this.request('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'kite-bench', version: '0' } });
    this.notify('notifications/initialized', {});
    this.tools = (await this.request('tools/list', {})).tools;
  }
  async call(name, args) {
    const r = await this.request('tools/call', { name, arguments: args });
    const text = (r.content || []).filter((c) => c.type === 'text').map((c) => c.text).join('\n');
    return { text, isError: !!r.isError };
  }
  close() { this.proc.kill(); }
}

export function devtoolsMcpCommand() {
  // Prefer the version already cached by Claude Code's npx, so runs are reproducible.
  const npx = join(homedir(), '.npm/_npx');
  if (existsSync(npx)) {
    for (const d of readdirSync(npx)) {
      const bin = join(npx, d, 'node_modules/chrome-devtools-mcp/build/src/bin/chrome-devtools-mcp.js');
      if (existsSync(bin)) {
        const version = JSON.parse(readFileSync(join(npx, d, 'node_modules/chrome-devtools-mcp/package.json'), 'utf8')).version;
        return { cmd: process.execPath, args: [bin], version };
      }
    }
  }
  return { cmd: 'npx', args: ['-y', 'chrome-devtools-mcp@latest'], version: 'latest' };
}

// ------------------------------------------------------------------ kite
export function kite(session, args, { quiet = false } = {}) {
  return new Promise((res) => {
    const p = spawn(KITE, ['-s', session, ...(quiet ? ['-q'] : []), ...args], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    p.stdout.on('data', (d) => (out += d));
    p.stderr.on('data', (d) => (out += d));
    p.on('close', (code) => res({ text: out.trimEnd(), code }));
  });
}

export const est = (bytes) => Math.round(bytes / 4);
export const sh = (s) => (/^[\w@:./=-]+$/.test(s) ? s : JSON.stringify(s));
export { execFileSync };
