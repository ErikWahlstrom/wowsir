#!/usr/bin/env node
// wowsir vs chrome-devtools-mcp: run the same scenario the way an agent would
// with each tool, and measure what the agent pays (calls, tokens, time) and
// what it learns (pass/fail, errors surfaced).
//
//   ./bench/fetch-sites.sh   (once: W3C examples, served locally on 127.0.0.1:8982)
//   node bench/bench.mjs bench/scenarios/*.mjs [--runs 3] [--mcp] [--only wowsir|devtools] [--show]
import { Mcp, launchChrome, cdp, devtoolsMcpCommand, wowsir, est, sh, now, WOWSIR, APG, APG_PORT } from './lib.mjs';
import { createServer } from 'node:http';
import { readFileSync, writeFileSync, mkdirSync, readdirSync, existsSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { resolve, basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const flag = (n, d) => { const i = argv.indexOf('--' + n); return i >= 0 ? argv[i + 1] : d; };
const has = (n) => argv.includes('--' + n);
const files = argv.filter((a, i) => !a.startsWith('--') && !(i > 0 && argv[i - 1].startsWith('--') && !['--show'].includes(argv[i - 1])));
const RUNS = +flag('runs', 1);
// Saved logins: wowsir loads ~/.wowsir/auth/*.json into every session by itself;
// the devtools driver gets the same cookies seeded into its Chrome.
const AUTH_DIR = process.env.WOWSIR_AUTH_DIR || join(homedir(), '.wowsir/auth');
const authCookies = () => existsSync(AUTH_DIR)
  ? readdirSync(AUTH_DIR).filter((f) => f.endsWith('.json')).flatMap((f) => JSON.parse(readFileSync(join(AUTH_DIR, f), 'utf8')).cookies || [])
  : [];
// Optional guard: BENCH_HOSTS=a.com,b.com refuses scenarios that open any other host.
const ALLOWED_HOSTS = process.env.BENCH_HOSTS ? process.env.BENCH_HOSTS.split(',') : null;
function checkHosts(scenario) {
  if (ALLOWED_HOSTS) for (const st of scenario.steps) if (st.url) {
    const h = new URL(st.url).host;
    if (!ALLOWED_HOSTS.includes(h)) throw new Error(`${scenario.name}: ${h} is not an allowed host (${ALLOWED_HOSTS.join(', ')})`);
  }
}
const ONLY = flag('only');
const SHOW = has('show');
const TOOLS = has('mcp') ? ['wowsir', 'wowsir-mcp', 'devtools'] : ['wowsir', 'devtools'];

// ------------------------------------------------------------------ helpers shared by drivers
const norm = (s) => (s || '').toLowerCase().replace(/[\u200b-\u200d\u2060\ufeff\ue000-\uf8ff]/g, '').replace(/\s+/g, ' ').trim();
// Find an element line for {role, name} in a tool's text output. `parse` yields {role, name, id}.
function find(items, role, name) {
  const r = role && norm(role), n = norm(name);
  const roleOk = (x) => !r || norm(x.role) === r || (r === 'textbox' && /searchbox|textbox|combobox/.test(norm(x.role)));
  return items.find((x) => roleOk(x) && norm(x.name) === n) || items.find((x) => roleOk(x) && norm(x.name).includes(n));
}
const wowsirItems = (text) => [...text.matchAll(/^[ +\-~]*([\w-]+) "((?:[^"\\]|\\.)*)" @e(\d+)/gm)].map((m) => ({ role: m[1], name: m[2], id: '@e' + m[3] }));
// icon buttons often have an empty name and their label in description="..." (an agent would read it)
const dtItems = (text) => [...text.matchAll(/uid=(\S+) ([\w-]+)(?: "((?:[^"\\]|\\.)*)")?([^\n]*)/g)].map((m) => {
  const desc = /description="((?:[^"\\]|\\.)*)"/.exec(m[4] || '');
  const name = (m[3] || '').replace(/[\u200b-\u200d\u2060\ufeff\ue000-\uf8ff]/g, '').trim(); // zero-width / icon-font (private use) glyphs aren't text
  return { role: m[2], name: name || (desc ? desc[1] : ''), id: m[1] };
});
// Errors an agent would notice in a response.
const wowsirIssues = (t) => t.split('\n').filter((l) => l.startsWith('! ')).map((l) => l.slice(2));
const dtIssues = (t) => t.split('\n').filter((l) => /^msgid=|\[error\]|\[(4\d\d|5\d\d)\]|\[failed|net::ERR/i.test(l.trim()));

class Recorder {
  // stale = an action's response showed a different page than the one the tool had navigated to
  constructor(tool) { this.tool = tool; this.calls = []; this.issues = new Set(); this.stale = 0; this.checked = 0; this.viewUrl = ''; }
  judge(contains) { this.checked++; if (!this.viewUrl.includes(contains)) this.stale++; }
  async run(name, fn) {
    const t0 = now();
    const r = await fn();
    const ms = now() - t0;
    this.calls.push({ name, ms, bytes: Buffer.byteLength(r.text || '') });
    if (SHOW) console.log(`\n── ${this.tool} · ${name} · ${Math.round(ms)}ms · ${Buffer.byteLength(r.text)}B\n${r.text}`);
    return r;
  }
}

// ------------------------------------------------------------------ wowsir driver
// transport 'cli' spawns `wowsir` per call; 'mcp' calls the `browser` tool of `wowsir mcp` (same output by design)
async function wowsirDriver(scenario, transport = 'cli') {
  const S = 'bench-' + Math.random().toString(36).slice(2, 8);
  // warm session (not counted); it loads saved logins unless the scenario must run logged out
  await wowsir(S, [...(scenario.auth === false ? ['--no-auth'] : []), 'open', 'about:blank'], { quiet: true });
  let mcp = null;
  if (transport === 'mcp') { mcp = new Mcp(WOWSIR, ['mcp']); await mcp.init(); }
  const exec = transport === 'mcp'
    ? async (args) => { const r = await mcp.call('browser', { command: args.map(sh).join(' '), session: S }); return { text: r.text, code: r.isError ? 1 : 0 }; }
    : (args) => wowsir(S, args);
  const rec = new Recorder(transport === 'mcp' ? 'wowsir-mcp' : 'wowsir');
  let last = '';
  const k = async (label, args) => {
    const r = await rec.run(label, () => exec(args));
    last = r.text.includes('@e') ? r.text : last;
    const u = /^→ (\S+)/m.exec(r.text) || /^# .* · (\S+)$/m.exec(r.text);
    if (u) rec.viewUrl = u[1];
    for (const i of wowsirIssues(r.text)) rec.issues.add(i);
    return r;
  };
  const target = async (role, name) => {
    const hit = find(wowsirItems(last), role, name);
    return hit ? hit.id : `${role || 'text'}${role ? ':' : '='}${name}`;
  };
  const failures = [];
  for (const st of scenario.steps) {
    let r;
    switch (st.do) {
      case 'open': r = await k('open', ['open', st.url]); break;
      case 'click': r = await k(`click ${st.name}`, ['click', await target(st.role, st.name)]); break;
      case 'fill': r = await k(`fill ${st.name}`, ['fill', await target(st.role, st.name), st.value]); break;
      case 'press': r = await k(`press ${st.key}`, ['press', st.key]); break;
      case 'select': r = await k(`select ${st.name}`, ['select', await target(st.role, st.name), st.value]); break;
      case 'expectText': r = await k(`expect text`, ['expect', 'text', st.text]); break;
      case 'expectUrl': rec.judge(st.contains); r = await k(`expect url`, ['expect', 'url', st.contains]); break;
      case 'look': r = await k(`snap`, ['snap', ...(st.interactive ? ['-i'] : [])]); break;
      case 'viewport': r = await k(`viewport`, ['viewport', String(st.width), String(st.height), ...(st.mobile ? ['--mobile'] : [])]); break;
      case 'errors': r = await k('errors', ['errors']); break;
      default: throw new Error('unknown step ' + st.do);
    }
    if (r.code !== 0 && st.do !== 'errors') failures.push(`${st.do} ${st.name || st.text || st.url || st.contains || ''}: ${r.text.split('\n')[0]}`);
    if (r.code !== 0 && st.must !== false && st.do !== 'errors') break;
  }
  await wowsir(S, ['close']);
  if (mcp) mcp.close();
  return { rec, failures };
}

// ------------------------------------------------------------------ chrome-devtools-mcp driver
async function devtoolsDriver(scenario, dt) {
  const chrome = await launchChrome();
  const cookies = scenario.auth === false ? [] : authCookies();
  if (cookies.length) await cdp(chrome.wsUrl, [['Storage.setCookies', { cookies }]]);
  const mcp = new Mcp(dt.cmd, [...dt.args, '--browserUrl', chrome.browserUrl, '--usageStatistics=false', '--performanceCrux=false']);
  await mcp.init(); // not counted: server startup
  const rec = new Recorder('devtools');
  const pageId = 1;
  let last = '';
  const c = async (label, tool, args) => {
    const r = await rec.run(label, () => mcp.call(tool, tool === 'list_pages' ? args : { pageId, ...args }));
    if (r.text.includes('uid=')) last = r.text;
    const u = /RootWebArea "[^\n]*?" url="([^"]+)"/.exec(r.text);
    if (u && tool !== 'list_pages') rec.viewUrl = u[1];
    for (const i of dtIssues(r.text)) rec.issues.add(i.trim().slice(0, 160));
    return r;
  };
  // An agent needs a uid from a snapshot; take a fresh one if the element isn't in the last one.
  const uid = async (role, name) => {
    let hit = find(dtItems(last), role, name);
    if (!hit) { await c('take_snapshot', 'take_snapshot', {}); hit = find(dtItems(last), role, name); }
    return hit && hit.id;
  };
  const failures = [];
  const fail = (m) => failures.push(m);
  outer: for (const st of scenario.steps) {
    let r, id;
    switch (st.do) {
      case 'open':
        r = await c('navigate_page', 'navigate_page', { type: 'url', url: st.url });
        if (r.isError) { fail(`open: ${r.text.slice(0, 120)}`); break outer; }
        await c('take_snapshot', 'take_snapshot', {}); // agent needs to see the page
        break;
      case 'click':
      case 'fill':
      case 'select':
        id = await uid(st.role, st.name);
        if (!id) { fail(`${st.do} ${st.name}: not found in snapshot`); if (st.must !== false) break outer; else break; }
        r = st.do === 'click'
          ? await c(`click ${st.name}`, 'click', { uid: id, includeSnapshot: true })
          : await c(`fill ${st.name}`, 'fill', { uid: id, value: st.value, includeSnapshot: true });
        if (r.isError) { fail(`${st.do} ${st.name}: ${r.text.slice(0, 120)}`); if (st.must !== false) break outer; }
        break;
      case 'press':
        r = await c(`press ${st.key}`, 'press_key', { key: st.key, includeSnapshot: true });
        break;
      case 'expectText':
        r = await c('wait_for', 'wait_for', { text: [st.text], timeout: 5000 });
        if (r.isError) { fail(`expectText ${st.text}: ${r.text.slice(0, 120)}`); if (st.must !== false) break outer; }
        break;
      case 'expectUrl': {
        rec.judge(st.contains);
        // click returns before an SPA navigation lands, so an agent re-checks (each check is a call)
        const t0 = now();
        do { r = await c('list_pages', 'list_pages', {}); if (r.text.includes(st.contains)) break; await new Promise((z) => setTimeout(z, 250)); } while (now() - t0 < 5000);
        if (!r.text.includes(st.contains)) { fail(`expectUrl ${st.contains}`); if (st.must !== false) break outer; }
        break;
      }
      case 'look':
        await c('take_snapshot', 'take_snapshot', {});
        break;
      case 'viewport':
        await c('resize_page', 'resize_page', { width: st.width, height: st.height });
        await c('take_snapshot', 'take_snapshot', {}); // see the responsive layout
        break;
      case 'errors':
        // best practice for this tool: keep messages across navigations, skip static assets
        await c('list_console_messages', 'list_console_messages', { types: ['error'], includePreservedMessages: true });
        await c('list_network_requests', 'list_network_requests', { resourceTypes: ['document', 'xhr', 'fetch'], includePreservedRequests: true });
        break;
      default: throw new Error('unknown step ' + st.do);
    }
  }
  mcp.close();
  chrome.kill();
  return { rec, failures };
}

// ------------------------------------------------------------------ report
const median = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.floor(s.length / 2)]; };
// scenario.noise: a regex for known third-party failures (e.g. analytics) left out of the error counts
let noise = null;
function summarize(runs) {
  const calls = median(runs.map((r) => r.rec.calls.length));
  const bytes = median(runs.map((r) => r.rec.calls.reduce((s, c) => s + c.bytes, 0)));
  const ms = median(runs.map((r) => r.rec.calls.reduce((s, c) => s + c.ms, 0)));
  const passed = runs.filter((r) => r.failures.length === 0).length;
  const stale = median(runs.map((r) => r.rec.stale)), checked = median(runs.map((r) => r.rec.checked));
  const issues = new Set(runs.flatMap((r) => [...r.rec.issues]).filter((i) => !(noise && noise.test(i))));
  return { calls, bytes, tokens: est(bytes), ms: Math.round(ms), passed, runs: runs.length, stale, checked, issues: [...issues], failures: runs.flatMap((r) => r.failures) };
}

// Static server for the W3C examples (only started when a scenario uses them).
const MIME = { html: 'text/html', js: 'text/javascript', mjs: 'text/javascript', css: 'text/css', svg: 'image/svg+xml', png: 'image/png', jpg: 'image/jpeg', json: 'application/json', woff2: 'font/woff2' };
let apgServer = null;
function serveApg() {
  if (apgServer) return;
  const root = join(here, 'sites', 'aria-practices');
  if (!existsSync(root)) { console.error('W3C examples missing: run bench/fetch-sites.sh first'); process.exit(1); }
  apgServer = createServer((req, res) => {
    const p = join(root, decodeURIComponent(new URL(req.url, 'http://x').pathname));
    if (!p.startsWith(root) || !existsSync(p) || !statSync(p).isFile()) { res.writeHead(404); return res.end(); }
    res.writeHead(200, { 'content-type': MIME[p.split('.').pop()] || 'application/octet-stream' });
    res.end(readFileSync(p));
  }).listen(APG_PORT, '127.0.0.1');
}

const dt = devtoolsMcpCommand();
const allResults = [];
for (const f of files) {
  const path = resolve(process.cwd(), f);
  const scenario = (await import(path)).default;
  checkHosts(scenario);
  if (scenario.steps.some((st) => st.url && st.url.startsWith(APG))) serveApg();
  if (scenario.auth && !authCookies().length) { console.log(`skip ${scenario.name}: needs a saved login (wowsir login <url> or wowsir login --from http://127.0.0.1:9222 <domain>)`); continue; }
  noise = scenario.noise || null;
  const res = { scenario: scenario.name, file: basename(f) };
  for (const tool of TOOLS) {
    if (ONLY && ONLY !== tool) continue;
    const runs = [];
    for (let i = 0; i < RUNS; i++) runs.push(tool === 'devtools' ? await devtoolsDriver(scenario, dt) : await wowsirDriver(scenario, tool === 'wowsir-mcp' ? 'mcp' : 'cli'));
    res[tool] = summarize(runs);
  }
  allResults.push(res);
}

const pct = (a, b) => (b ? `${Math.round((a / b) * 100)}%` : '–');
let md = `# wowsir vs chrome-devtools-mcp ${dt.version}\n\n${new Date().toISOString().slice(0, 16)} · runs per tool: ${RUNS} (medians)\n\n` +
  `*stale views*: URL checks where the preceding action's response still showed a different page than the one reached (the agent was looking at the wrong page).\n\n`;
md += '| scenario | tool | pass | calls | tokens (est) | time | stale views | errors surfaced |\n|---|---|---|---|---|---|---|---|\n';
for (const r of allResults) {
  for (const tool of TOOLS) {
    const s = r[tool]; if (!s) continue;
    md += `| ${r.scenario} | ${tool} | ${s.passed}/${s.runs} | ${s.calls} | ${s.tokens} | ${s.ms}ms | ${s.checked ? `${s.stale}/${s.checked}` : '–'} | ${s.issues.length} |\n`;
  }
  if (r.wowsir && r.devtools) md += `| | **wowsir/devtools** | | ${pct(r.wowsir.calls, r.devtools.calls)} | ${pct(r.wowsir.tokens, r.devtools.tokens)} | ${pct(r.wowsir.ms, r.devtools.ms)} | | |\n`;
}
for (const r of allResults) for (const tool of TOOLS) {
  const s = r[tool]; if (!s) continue;
  if (s.failures.length) md += `\n**${r.scenario} / ${tool} failures:**\n${[...new Set(s.failures)].map((f) => `- ${f}`).join('\n')}\n`;
  if (s.issues.length) md += `\n<details><summary>${r.scenario} / ${tool}: errors surfaced (${s.issues.length})</summary>\n\n${s.issues.map((i) => `- \`${i.replace(/`/g, "'")}\``).join('\n')}\n</details>\n`;
}
console.log(md);
mkdirSync(join(here, 'results'), { recursive: true });
const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
writeFileSync(join(here, 'results', `${stamp}.md`), md);
writeFileSync(join(here, 'results', `${stamp}.json`), JSON.stringify(allResults, null, 2));
apgServer?.close();
process.exit(0);
