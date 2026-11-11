//! A session = an isolated browser context with one page, plus everything we
//! track about it (network, console, dialogs) so each command can report
//! "what happened" without the agent having to ask.

use crate::cdp::{Cdp, Event};
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const LIB: &str = include_str!("inject.js");

/// Main-world shim: counts pending short timeouts so "settled" can wait for
/// `setTimeout(render, 120)`-style UI updates. Timers created inside a timer
/// callback are ignored, so polling loops never block settling.
const TIMER_SHIM: &str = r#"(() => {
  if (window.__wowsir_t) return;
  // A pending timer only counts if the action (or a fetch/XHR it caused)
  // plausibly created it: born within 600ms of the action mark, or within
  // 500ms after a request completed. Timers driven by live streams
  // (SSE/websocket ticks) or created inside timer callbacks don't count.
  const pending = new Map(); let depth = 0; let mark = 0; const done = [];
  const now = () => performance.now();
  const st = window.setTimeout, ct = window.clearTimeout;
  window.setTimeout = function (fn, ms, ...rest) {
    const d = +ms || 0;
    const track = depth === 0 && d <= 1000 && typeof fn === 'function';
    let id;
    id = st.call(this, function () { pending.delete(id); depth++; try { return fn.apply(this, arguments); } finally { depth--; } }, ms, ...rest);
    if (track) pending.set(id, { due: now() + d, born: now() });
    return id;
  };
  window.clearTimeout = function (id) { pending.delete(id); return ct.call(this, id); };
  const finished = () => { done.push(now()); if (done.length > 50) done.shift(); };
  const f = window.fetch;
  if (f) window.fetch = function () { const p = f.apply(this, arguments); p.then(finished, finished); return p; };
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function () { this.addEventListener('loadend', finished); return send.apply(this, arguments); };
  const caused = (born) => (born >= mark && born - mark < 600) || done.some((c) => c >= mark && born >= c && born - c < 500);
  Object.defineProperty(window, '__wowsir_mark', { value: () => { mark = now(); }, enumerable: false });
  Object.defineProperty(window, '__wowsir_t', { value: () => { const t = now(); let n = 0; for (const [id, p] of pending) { if (p.due < t - 2000) pending.delete(id); else if (caused(p.born)) n++; } return n; }, enumerable: false });
})();"#;

pub struct Req {
    id: String,
    method: String,
    url: String,
    rtype: String,
    /// frame that made the request
    frame: String,
    start: Instant,
    status: i64,
}

/// A network mock: requests whose URL matches `pattern` get a canned response.
#[derive(Clone, Debug)]
pub struct Mock {
    pub pattern: String,
    /// 0 = let it through (after `delay_ms`)
    pub status: u16,
    pub body: Vec<u8>,
    pub ctype: String,
    pub abort: bool,
    pub delay_ms: u64,
    /// also mock top-level page loads (off by default so `/assets` doesn't swallow the page)
    pub doc: bool,
    pub hits: u64,
}

/// `*` and `?` wildcards; a pattern without `*` matches anywhere in the URL.
pub fn url_glob(pattern: &str) -> String {
    if pattern.contains('*') { pattern.to_string() } else { format!("*{pattern}*") }
}

pub fn glob_match(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub struct State {
    pub inflight: HashMap<String, Req>,
    last_net_change: Instant,
    /// notable things not yet reported to the agent
    pub events: Vec<String>,
    /// all errors since last `open` / `errors --clear`
    pub errors: Vec<String>,
    pub console: VecDeque<String>,
    pub net: VecDeque<String>,
    /// (requestId, log line) of completed responses, for `net --body`
    pub done: VecDeque<(String, String)>,
    pub url: String,
    pub loading: bool,
    pub doc_status: i64,
    /// increments on every new main-frame document (not on pushState)
    pub doc_seq: u64,
    pub world: Option<i64>,
    /// cross-origin iframes and workers attached under this page (their own CDP sessions)
    pub children: Vec<Child>,
    /// console.warn messages since the last action report
    pub warnings: Vec<String>,
    /// wowsir's isolated world per cross-origin frame: frame id -> (session id, context id)
    pub frame_worlds: HashMap<String, (String, i64)>,
    /// the page has shown a cross-origin iframe: route lookups into frames too
    pub xframes: bool,
    pub dialog_accept: bool,
    /// text typed into prompt() dialogs (`dialog accept TEXT`)
    pub dialog_text: Option<String>,
    pub block_types: Vec<String>,
    pub mocks: Vec<Mock>,
    /// network request ids a mock is deliberately holding back (never block settling)
    pub delayed: std::collections::HashSet<String>,
}

impl State {
    fn new() -> Self {
        State {
            inflight: HashMap::new(),
            last_net_change: Instant::now(),
            events: vec![],
            errors: vec![],
            console: VecDeque::new(),
            net: VecDeque::new(),
            done: VecDeque::new(),
            url: "about:blank".into(),
            loading: false,
            doc_status: 0,
            doc_seq: 0,
            world: None,
            children: vec![],
            warnings: vec![],
            frame_worlds: HashMap::new(),
            xframes: false,
            dialog_accept: true,
            dialog_text: None,
            block_types: vec![],
            mocks: vec![],
            delayed: Default::default(),
        }
    }

    /// Requests that should block "settled": not streams, not long-polls.
    fn busy(&self) -> Vec<&Req> {
        self.inflight
            .values()
            .filter(|r| {
                !matches!(r.rtype.as_str(), "EventSource" | "WebSocket" | "Media" | "Ping" | "Prefetch" | "Image" | "Font")
                    && r.start.elapsed() < Duration::from_millis(2500)
            })
            .filter(|r| !self.delayed.contains(&r.id))
            .collect()
    }

    /// note_error, tagged with the iframe/worker it came from
    fn note_error_in(&mut self, label: &Option<String>, s: String) {
        match label {
            Some(l) => self.note_error(format!("{s} (in {l})")),
            None => self.note_error(s),
        }
    }

    fn note_error(&mut self, s: String) {
        self.events.push(s.clone());
        if self.errors.len() < 200 {
            self.errors.push(s);
        }
    }

    pub fn short_url(&self, u: &str) -> String {
        short_url(&self.url, u)
    }
}

/// Insert a frame's snapshot lines under its <iframe @eN> line, indented one level deeper
/// (at the end if the iframe line was filtered out).
fn splice(lines: &mut Vec<String>, owner: u64, sub: &[String]) {
    if sub.is_empty() {
        return;
    }
    let tag = format!("@e{owner}");
    let at = lines.iter().position(|l| l.split_whitespace().any(|w| w == tag));
    let (idx, pad) = match at {
        Some(i) => (i + 1, " ".repeat(lines[i].len() - lines[i].trim_start().len() + 2)),
        None => (lines.len(), String::new()),
    };
    let ins: Vec<String> = sub.iter().map(|l| format!("{pad}{l}")).collect();
    lines.splice(idx..idx, ins);
}

/// A cross-origin iframe (out-of-process) or a worker, attached through Target.setAutoAttach.
#[derive(Clone, Debug)]
pub struct Child {
    pub sid: String,
    pub target: String,
    /// session id of the page or frame it belongs to
    pub parent: String,
    /// "iframe" | "worker" | "shared_worker"
    pub kind: String,
}

/// A cross-origin iframe wowsir can look into: its frame id plus the CDP session
/// that runs it (the page's own session in chrome-headless-shell, which keeps
/// cross-origin frames in-process; the frame's own session when it is out-of-process).
#[derive(Clone, Debug)]
pub struct XFrame {
    pub frame_id: String,
    pub sid: String,
}

/// Mocks and blocked resource types for one session (the page, or an out-of-process frame).
async fn fetch_setup(cdp: &Arc<Cdp>, sid: &str, st: &Arc<Mutex<State>>) -> Result<()> {
    let (patterns, mocking): (Vec<Value>, bool) = {
        let s = st.lock().unwrap();
        (
            s.block_types
                .iter()
                .map(|t| json!({ "resourceType": t, "requestStage": "Request" }))
                .chain(s.mocks.iter().map(|m| json!({ "urlPattern": url_glob(&m.pattern), "requestStage": "Request" })))
                .collect(),
            !s.mocks.is_empty(),
        )
    };
    let _ = cdp.call(Some(sid), "Network.setCacheDisabled", json!({ "cacheDisabled": mocking })).await;
    if patterns.is_empty() {
        cdp.call(Some(sid), "Fetch.disable", json!({})).await?;
    } else {
        cdp.call(Some(sid), "Fetch.enable", json!({ "patterns": patterns })).await?;
    }
    Ok(())
}

/// Children start paused until listeners (and mocks) are set up, then attach_child resumes them.
fn auto_attach() -> Value {
    json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true,
            "filter": [{ "type": "iframe" }, { "type": "worker" }, { "type": "shared_worker" }, { "exclude": true }] })
}

/// "iframe pay.example.com" / "worker sw.js": where an error came from
fn child_label(kind: &str, url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let what = if kind == "iframe" {
        rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
    } else {
        let path = rest.split(['?', '#']).next().unwrap_or(rest);
        path.rsplit('/').next().filter(|f| !f.is_empty()).unwrap_or(path).to_string()
    };
    format!("{} {}", if kind == "iframe" { "iframe" } else { "worker" }, clip(&what, 60))
}

/// Set up a child session reported by Target.attachedToTarget: forward its errors,
/// network and dialogs into the page's state; frames auto-attach their own children.
fn attach_child(cdp: Arc<Cdp>, parent: String, st: Arc<Mutex<State>>, p: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let Some(csid) = p["sessionId"].as_str().map(String::from) else { return };
        let info = &p["targetInfo"];
        let kind = info["type"].as_str().unwrap_or("").to_string();
        let target = info["targetId"].as_str().unwrap_or("").to_string();
        let url = info["url"].as_str().unwrap_or("").to_string();
        let label = child_label(&kind, &url);
        {
            let mut s = st.lock().unwrap();
            s.children.push(Child { sid: csid.clone(), target: target.clone(), parent, kind: kind.clone() });
            if kind == "iframe" {
                // an out-of-process frame's document request starts on the parent and
                // finishes in the frame's own session
                s.inflight.retain(|_, r| !(r.frame == target && r.rtype == "Document"));
                s.last_net_change = Instant::now();
            } else if !url.is_empty() {
                // the worker's script request started on the page but finishes in the worker's
                // own session (often before we listen there): a running worker means it loaded
                s.inflight.retain(|_, r| r.url != url);
                s.last_net_change = Instant::now();
            }
        }
        let rx = cdp.subscribe(&csid);
        let label = Arc::new(Mutex::new(label));
        tokio::spawn(event_loop(cdp.clone(), csid.clone(), target, st.clone(), rx, Some(label.clone())));
        let s = Some(csid.as_str());
        // name the frame first: Runtime.enable replays console messages logged before we attached
        if kind == "iframe" {
            let _ = cdp.call(s, "Page.enable", json!({})).await;
            if let Ok(t) = cdp.call(s, "Page.getFrameTree", json!({})).await {
                if let Some(u) = t["frameTree"]["frame"]["url"].as_str().filter(|u| !u.is_empty()) {
                    *label.lock().unwrap() = child_label("iframe", u);
                }
            }
            let _ = cdp.call(s, "Target.setAutoAttach", auto_attach()).await;
        }
        let _ = cdp.call(s, "Runtime.enable", json!({})).await;
        let _ = cdp.call(s, "Network.enable", json!({})).await;
        let _ = cdp.call(s, "Log.enable", json!({})).await;
        if kind == "iframe" {
            let active = { let x = st.lock().unwrap(); !x.mocks.is_empty() || !x.block_types.is_empty() };
            if active {
                let _ = fetch_setup(&cdp, &csid, &st).await;
            }
        }
        let _ = cdp.call(s, "Runtime.runIfWaitingForDebugger", json!({})).await;
    })
}

pub fn short_url(page: &str, u: &str) -> String {
    let origin = |s: &str| -> Option<String> {
        let i = s.find("://")?;
        let rest = &s[i + 3..];
        let end = rest.find('/').unwrap_or(rest.len());
        Some(s[..i + 3 + end].to_string())
    };
    let mut out = match (origin(page), origin(u)) {
        (Some(a), Some(b)) if a == b => {
            let p = &u[a.len()..];
            if p.is_empty() { "/".to_string() } else { p.to_string() }
        }
        _ => u.to_string(),
    };
    if out.starts_with("data:") && out.len() > 40 {
        out.truncate(40);
        out.push('…');
    }
    if out.chars().count() > 140 {
        out = out.chars().take(139).collect::<String>() + "…";
    }
    out
}

#[derive(Deserialize, Default, Debug)]
pub struct Snap {
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub more: usize,
    #[serde(default)]
    pub base: Option<Vec<String>>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub next: u64,
}

#[derive(Clone, Debug)]
pub struct Watch {
    pub target: String,
    pub sel: String,
    pub desc: String,
    pub base: Option<[i64; 4]>,
}

/// Browser-level page lifecycle, routed to the session owning the context.
pub enum TabEvent {
    Created { target: String, url: String },
    Destroyed(String),
}

/// A page in the session that is not the active one.
pub struct Tab {
    pub target: String,
    pub sid: String,
    pub st: Arc<Mutex<State>>,
    pub baseline: Option<Vec<String>>,
}

pub struct Session {
    pub name: String,
    pub cdp: Arc<Cdp>,
    pub browser_ctx: String,
    pub target: String,
    pub sid: String,
    pub st: Arc<Mutex<State>>,
    /// last full snapshot the agent saw (diff base)
    pub baseline: Option<Vec<String>>,
    /// replayable command log
    pub history: Vec<String>,
    pub next_ref: u64,
    pub timeout: Duration,
    /// URL of the last `open` (for "sign in at" hints)
    pub last_open: String,
    pub warned_401: bool,
    pub last_writeback: Instant,
    /// `watch`: (target as given, stable selector, last known box)
    pub watches: Vec<Watch>,
    /// movements seen since the last `expect still`
    pub moves: Vec<(String, String)>,
    /// page the current recording started on (for `record save` without an `open`)
    pub record_url: String,
    /// modifier bits held during the next mouse click (Alt=1 Ctrl=2 Meta=4 Shift=8)
    pub click_mods: i64,
    /// saved-login expiry warnings, shown once the agent is on that site: (site, message)
    pub expiry_notes: Vec<(String, String)>,
    /// other open pages (popups, target=_blank); the active one lives in target/sid/st
    pub tabs: Vec<Tab>,
    /// creation order of all pages, for stable `tab N` numbering
    pub tab_order: Vec<String>,
    pub tab_events: Arc<Mutex<Vec<TabEvent>>>,
}

/// Attach to a page target and start tracking it.
async fn attach_page(cdp: &Arc<Cdp>, target: &str) -> Result<(String, Arc<Mutex<State>>)> {
    let sid = cdp
        .call(None, "Target.attachToTarget", json!({ "targetId": target, "flatten": true }))
        .await?["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow!("no sessionId"))?
        .to_string();
    let rx = cdp.subscribe(&sid);
    let st = Arc::new(Mutex::new(State::new()));
    tokio::spawn(event_loop(cdp.clone(), sid.clone(), target.to_string(), st.clone(), rx, None));
    let s = Some(sid.as_str());
    tokio::try_join!(
        cdp.call(s, "Page.enable", json!({})),
        cdp.call(s, "Runtime.enable", json!({})),
        cdp.call(s, "Network.enable", json!({})),
        cdp.call(s, "Log.enable", json!({})),
        cdp.call(
            s,
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": false})
        ),
        cdp.call(s, "Emulation.setFocusEmulationEnabled", json!({ "enabled": true })),
        cdp.call(s, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": TIMER_SHIM })),
    )?;
    // cross-origin iframes and workers run in other processes: attach to them too
    let _ = cdp.call(s, "Target.setAutoAttach", auto_attach()).await;
    Ok((sid, st))
}

impl Session {
    pub async fn create(cdp: Arc<Cdp>, name: &str) -> Result<Session> {
        let ctx = cdp
            .call(None, "Target.createBrowserContext", json!({ "disposeOnDetach": true }))
            .await?["browserContextId"]
            .as_str()
            .ok_or_else(|| anyhow!("no browserContextId"))?
            .to_string();
        let target = cdp
            .call(None, "Target.createTarget", json!({ "url": "about:blank", "browserContextId": ctx }))
            .await?["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("no targetId"))?
            .to_string();
        let (sid, st) = attach_page(&cdp, &target).await?;
        Ok(Session {
            name: name.to_string(),
            cdp,
            browser_ctx: ctx,
            target: target.clone(),
            sid,
            st,
            baseline: None,
            history: vec![],
            next_ref: 1,
            last_open: String::new(),
            warned_401: false,
            last_writeback: Instant::now(),
            click_mods: 0,
            record_url: String::new(),
            watches: vec![],
            moves: vec![],
            expiry_notes: vec![],
            tab_order: vec![target.clone()],
            tabs: vec![],
            tab_events: Arc::new(Mutex::new(vec![])),
            timeout: Duration::from_millis(
                std::env::var("WOWSIR_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(5000),
            ),
        })
    }

    pub async fn close(&self) {
        self.cdp.unsubscribe(&self.sid);
        for t in &self.tabs {
            self.cdp.unsubscribe(&t.sid);
        }
        let _ = self
            .cdp
            .call(None, "Target.disposeBrowserContext", json!({ "browserContextId": self.browser_ctx }))
            .await;
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.call(Some(&self.sid), method, params).await
    }

    async fn world(&mut self) -> Result<i64> {
        if let Some(id) = self.st.lock().unwrap().world {
            return Ok(id);
        }
        let r = self
            .call(
                "Page.createIsolatedWorld",
                json!({ "frameId": self.target, "worldName": "wowsir", "grantUniveralAccess": true }),
            )
            .await?;
        let id = r["executionContextId"].as_i64().ok_or_else(|| anyhow!("no isolated world"))?;
        let r = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": format!("{LIB}\n__wowsir.init({});", self.next_ref), "contextId": id }),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("failed to inject page library: {}", exception_text(ex));
        }
        self.st.lock().unwrap().world = Some(id);
        Ok(id)
    }

    /// Evaluate an expression in wowsir's isolated world. Retries across navigations.
    pub async fn js(&mut self, expr: &str) -> Result<Value> {
        self.js_at(None, expr).await
    }

    /// wowsir's isolated world inside a cross-origin frame (created and loaded on first use).
    async fn frame_world(&mut self, f: &XFrame) -> Result<i64> {
        if let Some((_, id)) = self.st.lock().unwrap().frame_worlds.get(&f.frame_id) {
            return Ok(*id);
        }
        let r = self
            .cdp
            .call(Some(&f.sid), "Page.createIsolatedWorld", json!({ "frameId": f.frame_id, "worldName": "wowsir", "grantUniveralAccess": true }))
            .await?;
        let id = r["executionContextId"].as_i64().ok_or_else(|| anyhow!("no isolated world in frame"))?;
        let r = self
            .cdp
            .call(Some(&f.sid), "Runtime.evaluate", json!({ "expression": format!("{LIB}\n__wowsir.init({});", self.next_ref), "contextId": id }))
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("failed to inject page library into frame: {}", exception_text(ex));
        }
        self.st.lock().unwrap().frame_worlds.insert(f.frame_id.clone(), (f.sid.clone(), id));
        Ok(id)
    }

    /// Evaluate in the page's world (frame None) or in a cross-origin frame's world.
    /// Once frames are involved, every call keeps the @e counter shared across worlds.
    async fn js_at(&mut self, frame: Option<&XFrame>, expr: &str) -> Result<Value> {
        let sync = frame.is_some() || self.st.lock().unwrap().xframes;
        let expr = if sync {
            format!("(async () => {{ __wowsir.init({}); const v = await ({expr}); return [v, __wowsir.nextRef()]; }})()", self.next_ref)
        } else {
            expr.to_string()
        };
        for _ in 0..4 {
            let ctx = match frame {
                None => self.world().await?,
                Some(f) => self.frame_world(f).await?,
            };
            let sid = frame.map_or(self.sid.clone(), |f| f.sid.clone());
            let r = self
                .cdp
                .call(
                    Some(&sid),
                    "Runtime.evaluate",
                    json!({ "expression": expr, "contextId": ctx, "returnByValue": true, "awaitPromise": true }),
                )
                .await;
            let forget = |me: &mut Self| {
                let mut st = me.st.lock().unwrap();
                match frame {
                    None => st.world = None,
                    Some(f) => {
                        st.frame_worlds.remove(&f.frame_id);
                    }
                }
            };
            match r {
                Err(e) => {
                    let m = e.to_string();
                    if m.contains("context") || m.contains("navigated") || m.contains("Promise was collected") {
                        forget(self);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                    return Err(e);
                }
                Ok(v) => {
                    if let Some(ex) = v.get("exceptionDetails") {
                        let t = exception_text(ex);
                        if t.contains("__wowsir is not defined") {
                            forget(self);
                            continue;
                        }
                        bail!("{t}");
                    }
                    let val = v["result"].get("value").cloned().unwrap_or(Value::Null);
                    if sync {
                        if let Some(n) = val[1].as_u64() {
                            self.next_ref = self.next_ref.max(n);
                        }
                        return Ok(val[0].clone());
                    }
                    return Ok(val);
                }
            }
        }
        bail!("page kept navigating while wowsir tried to read it")
    }

    /// Cross-origin iframes directly in the page, with the session that runs each.
    pub async fn xframes(&mut self) -> Vec<XFrame> {
        let Ok(t) = self.call("Page.getFrameTree", json!({})).await else { return vec![] };
        let origin = t["frameTree"]["frame"]["securityOrigin"].as_str().unwrap_or("").to_string();
        let children = self.st.lock().unwrap().children.clone();
        let mut out = vec![];
        for c in t["frameTree"]["childFrames"].as_array().cloned().unwrap_or_default() {
            let f = &c["frame"];
            let o = f["securityOrigin"].as_str().unwrap_or("");
            let Some(id) = f["id"].as_str() else { continue };
            if o == origin || o.is_empty() || o == "null" || o.starts_with("://") {
                continue; // same-origin (walked directly) or about:blank
            }
            let sid = children.iter().find(|k| k.target == id && k.kind == "iframe").map_or(self.sid.clone(), |k| k.sid.clone());
            out.push(XFrame { frame_id: id.to_string(), sid });
        }
        // out-of-process frames the tree didn't list
        for k in children.iter().filter(|k| k.kind == "iframe" && k.parent == self.sid) {
            if !out.iter().any(|f| f.frame_id == k.target) {
                out.push(XFrame { frame_id: k.target.clone(), sid: k.sid.clone() });
            }
        }
        out
    }

    /// Call `function(){...}` with `this` = the <iframe> element that owns frame `f`, in the page's world.
    async fn on_frame_owner(&mut self, f: &XFrame, func: &str) -> Result<Value> {
        let o = self.call("DOM.getFrameOwner", json!({ "frameId": f.frame_id })).await?;
        let ctx = self.world().await?;
        let r = self.call("DOM.resolveNode", json!({ "backendNodeId": o["backendNodeId"], "executionContextId": ctx })).await?;
        let id = r["object"]["objectId"].as_str().ok_or_else(|| anyhow!("frame owner not found"))?.to_string();
        let v = self
            .call("Runtime.callFunctionOn", json!({ "functionDeclaration": func, "objectId": id, "returnByValue": true }))
            .await?;
        let _ = self.call("Runtime.releaseObject", json!({ "objectId": id })).await;
        Ok(v["result"]["value"].clone())
    }

    /// Like `js` but returns a remote object id (for DOM.setFileInputFiles etc).
    pub async fn js_object(&mut self, expr: &str) -> Result<String> {
        let ctx = self.world().await?;
        let v = self
            .call("Runtime.evaluate", json!({ "expression": expr, "contextId": ctx, "returnByValue": false }))
            .await?;
        if let Some(ex) = v.get("exceptionDetails") {
            bail!("{}", exception_text(ex));
        }
        v["result"]["objectId"].as_str().map(String::from).ok_or_else(|| anyhow!("no object"))
    }

    /// Call `__wowsir.<f>(args...)` in the page; if the target isn't there and the page
    /// has cross-origin iframes, look in those (adding the frame's offset to points).
    pub async fn k(&mut self, f: &str, args: &[Value]) -> Result<Value> {
        let a: Vec<String> = args.iter().map(|v| v.to_string()).collect();
        let expr = format!("__wowsir.{f}({})", a.join(","));
        let r = self.js_at(None, &expr).await;
        if !self.st.lock().unwrap().xframes || !args.first().is_some_and(|v| v.is_string()) {
            return r;
        }
        let missing = match (&r, f) {
            (Ok(v), "hasText") => v.as_bool() == Some(false),
            (Ok(v), "count") => v.as_i64() == Some(0),
            (Ok(v), "probe") => v["found"] == false,
            // actionability checks report a missing element as {retry: ...} so callers keep polling
            (Ok(v), _) => v["retry"].as_str().is_some_and(|m| m.contains("no element matches") || m.contains("stale ref")),
            (Err(e), _) => {
                let m = e.to_string();
                m.contains("no element matches") || m.contains("stale ref")
            }
        };
        if !missing {
            return r;
        }
        for fr in self.xframes().await {
            let Ok(mut v) = self.js_at(Some(&fr), &expr).await else { continue };
            if v["retry"].as_str().is_some_and(|m| m.contains("no element matches") || m.contains("stale ref")) {
                continue;
            }
            match f {
                "probe" if v["found"] == false => continue,
                "hasText" if v.as_bool() != Some(true) => continue,
                "count" if v.as_i64().unwrap_or(0) == 0 => continue,
                "point" | "rect" if v.get("x").is_some() => {
                    // frame coordinates -> page coordinates (the frame's content box)
                    let b = self.on_frame_owner(&fr, "function(){ return __wowsir.frameBox(this) }").await?;
                    for (k, d) in [("x", "x"), ("y", "y")] {
                        if let Some(n) = v[k].as_f64() {
                            v[k] = json!(n + b[d].as_f64().unwrap_or(0.0));
                        }
                    }
                }
                _ => {}
            }
            return Ok(v);
        }
        r
    }

    /// Find closed shadow roots (invisible to page JS) and hand them to the page library.
    /// Only defined custom elements without an open root are asked about, once each.
    pub async fn register_closed(&mut self) -> Result<()> {
        for _ in 0..4 {
            // nested closed roots show up once their host's root is registered: loop a few rounds
            let ctx = self.world().await?;
            let v = self
                .call("Runtime.evaluate", json!({ "expression": "__wowsir.closedCandidates()", "contextId": ctx, "returnByValue": false }))
                .await?;
            let Some(arr) = v["result"]["objectId"].as_str().map(String::from) else { return Ok(()) };
            let props = self.call("Runtime.getProperties", json!({ "objectId": arr, "ownProperties": true })).await?;
            let hosts: Vec<String> = props["result"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|p| p["name"].as_str().is_some_and(|n| n.parse::<u32>().is_ok()))
                        .filter_map(|p| p["value"]["objectId"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let mut found = false;
            for host in hosts {
                let Ok(d) = self.call("DOM.describeNode", json!({ "objectId": host, "depth": 1, "pierce": true })).await else { continue };
                for sr in d["node"]["shadowRoots"].as_array().cloned().unwrap_or_default() {
                    if sr["shadowRootType"] != "closed" {
                        continue;
                    }
                    let Ok(r) = self
                        .call("DOM.resolveNode", json!({ "backendNodeId": sr["backendNodeId"], "executionContextId": ctx }))
                        .await
                    else {
                        continue;
                    };
                    if let Some(rid) = r["object"]["objectId"].as_str() {
                        self.call(
                            "Runtime.callFunctionOn",
                            json!({ "functionDeclaration": "function(r){ __wowsir.addClosed(this, r) }", "objectId": host,
                                    "arguments": [{ "objectId": rid }] }),
                        )
                        .await?;
                        found = true;
                    }
                }
            }
            let _ = self.call("Runtime.releaseObject", json!({ "objectId": arr })).await;
            if !found {
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn snapshot(&mut self, opts: Value) -> Result<Snap> {
        let _ = self.register_closed().await;
        let v = self.js_at(None, &format!("__wowsir.snapshot({opts})")).await?;
        let mut s: Snap = serde_json::from_value(v)?;
        self.next_ref = self.next_ref.max(s.next); // frames number their refs after the page's
        let marker = |l: &String| l.trim_start().starts_with("iframe") && l.ends_with("cross-origin");
        if s.lines.iter().any(marker) || s.base.as_ref().is_some_and(|b| b.iter().any(marker)) {
            if !self.st.lock().unwrap().xframes {
                // from now on calls keep the @e counter shared with frames: re-read the page's
                self.st.lock().unwrap().xframes = true;
            }
            let mut sub = opts.clone();
            if let Some(o) = sub.as_object_mut() {
                o.remove("in");
            }
            for fr in self.xframes().await {
                let Ok(owner) = self.on_frame_owner(&fr, "function(){ return __wowsir.refOf(this) }").await else { continue };
                let Some(n) = owner.as_u64() else { continue };
                let Ok(v) = self.js_at(Some(&fr), &format!("__wowsir.snapshot({sub})")).await else { continue };
                let Ok(cs) = serde_json::from_value::<Snap>(v) else { continue };
                splice(&mut s.lines, n, &cs.lines);
                if let (Some(b), Some(cb)) = (s.base.as_mut(), cs.base.as_ref()) {
                    splice(b, n, cb);
                }
                s.next = s.next.max(cs.next);
            }
        }
        if s.next > self.next_ref {
            self.next_ref = s.next;
        }
        Ok(s)
    }

    /// Wait until the page is quiet: no navigation in flight, no relevant
    /// network for 50ms, and no DOM mutations for 40ms. Returns a note if
    /// we gave up.
    pub async fn settle(&mut self, max: Duration) -> Option<String> {
        let start = Instant::now();
        loop {
            // the page closed itself (e.g. a sign-in popup): nothing to wait for
            if self.tab_events.lock().unwrap().iter().any(|e| matches!(e, TabEvent::Destroyed(t) if *t == self.target)) {
                return None;
            }
            let (ready, pending) = {
                let s = self.st.lock().unwrap();
                let busy = s.busy();
                let ready = !s.loading && busy.is_empty() && s.last_net_change.elapsed() >= Duration::from_millis(50);
                let pending: Vec<String> = busy.iter().take(3).map(|r| s.short_url(&r.url)).collect();
                (ready, pending)
            };
            if ready && start.elapsed() >= Duration::from_millis(25) {
                let timers = self
                    .call("Runtime.evaluate", json!({ "expression": "window.__wowsir_t ? window.__wowsir_t() : 0", "returnByValue": true }))
                    .await
                    .ok()
                    .and_then(|v| v["result"]["value"].as_i64())
                    .unwrap_or(0);
                if timers == 0 {
                    if let Ok(v) = self.js("__wowsir.quiet()").await {
                        if v.as_i64().unwrap_or(0) >= 40 {
                            return None;
                        }
                    }
                }
            }
            if start.elapsed() > max {
                let loading = self.st.lock().unwrap().loading;
                return Some(if loading {
                    format!("page still loading after {}ms", max.as_millis())
                } else if !pending.is_empty() {
                    format!("still busy after {}ms: {}", max.as_millis(), pending.join(", "))
                } else {
                    format!("DOM still changing after {}ms (animation?)", max.as_millis())
                });
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Make `t` the active page; returns the previously active one.
    fn swap_in(&mut self, mut t: Tab) -> Tab {
        std::mem::swap(&mut self.target, &mut t.target);
        std::mem::swap(&mut self.sid, &mut t.sid);
        std::mem::swap(&mut self.st, &mut t.st);
        std::mem::swap(&mut self.baseline, &mut t.baseline);
        // the agent last saw the other tab: show this one in full
        self.baseline = None;
        t
    }

    /// Switch to the page with this target id (must be in `tabs`).
    pub fn activate(&mut self, target: &str) -> bool {
        let Some(i) = self.tabs.iter().position(|t| t.target == target) else { return false };
        let t = self.tabs.remove(i);
        let old = self.swap_in(t);
        self.tabs.push(old);
        true
    }

    /// Close the active page and fall back to the most recent other one.
    pub async fn close_active(&mut self) -> Result<()> {
        let Some(next) = self.tabs.pop() else { bail!("only one tab open: use `wowsir close` to end the session") };
        let old = self.swap_in(next);
        self.cdp.unsubscribe(&old.sid);
        self.tab_order.retain(|t| *t != old.target);
        let _ = self.cdp.call(None, "Target.closeTarget", json!({ "targetId": old.target })).await;
        Ok(())
    }

    /// Open a new page in this session and make it active.
    pub async fn new_tab(&mut self) -> Result<()> {
        let target = self
            .cdp
            .call(None, "Target.createTarget", json!({ "url": "about:blank", "browserContextId": self.browser_ctx }))
            .await?["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("no targetId"))?
            .to_string();
        self.adopt(&target, "about:blank").await?;
        Ok(())
    }

    async fn adopt(&mut self, target: &str, url: &str) -> Result<()> {
        let (sid, st) = attach_page(&self.cdp, target).await?;
        let (blocking, mocks, accept, text) = {
            let s = self.st.lock().unwrap();
            (s.block_types.clone(), s.mocks.clone(), s.dialog_accept, s.dialog_text.clone())
        };
        {
            let mut s = st.lock().unwrap();
            s.dialog_accept = accept;
            s.dialog_text = text;
            s.url = url.to_string();
        }
        // we may have attached mid-load: find out where the page is
        if let Ok(v) = self
            .cdp
            .call(Some(&sid), "Runtime.evaluate", json!({ "expression": "[location.href, document.readyState]", "returnByValue": true }))
            .await
        {
            let href = v["result"]["value"][0].as_str().unwrap_or("").to_string();
            let ready = v["result"]["value"][1].as_str().unwrap_or("");
            let mut s = st.lock().unwrap();
            let pending_nav = href == "about:blank" && !url.is_empty() && url != "about:blank";
            if !pending_nav && !href.is_empty() {
                s.url = href;
            }
            if ready != "complete" || pending_nav {
                s.loading = true;
            }
        }
        let tab = Tab { target: target.to_string(), sid, st, baseline: None };
        let old = self.swap_in(tab);
        self.tabs.push(old);
        if !self.tab_order.iter().any(|t| t == target) {
            self.tab_order.push(target.to_string());
        }
        if !blocking.is_empty() || !mocks.is_empty() {
            {
                let mut s = self.st.lock().unwrap();
                s.block_types = blocking;
                s.mocks = mocks;
            }
            let _ = self.apply_fetch().await;
        }
        Ok(())
    }

    /// Follow pages opened or closed by the site. Notes go into the (new) active
    /// page's events so the next report includes them. Returns true if the
    /// active page changed.
    pub async fn sync_tabs(&mut self) -> bool {
        let evs: Vec<TabEvent> = std::mem::take(&mut *self.tab_events.lock().unwrap());
        let mut switched = false;
        for ev in evs {
            match ev {
                TabEvent::Created { target, url } => {
                    if target == self.target || self.tabs.iter().any(|t| t.target == target) {
                        continue;
                    }
                    if self.adopt(&target, &url).await.is_ok() {
                        switched = true;
                        let n = self.tab_order.len() - 1;
                        let back = self.tab_order.iter().position(|t| self.tabs.last().map_or(false, |l| &l.target == t)).unwrap_or(0);
                        self.st.lock().unwrap().events.push(format!(
                            "new tab {n} opened and is now active (`wowsir tab {back}` goes back, `wowsir tab close` closes it)"
                        ));
                    }
                }
                TabEvent::Destroyed(target) => {
                    if target == self.target {
                        let gone = self.url();
                        if let Some(next) = self.tabs.pop() {
                            let old = self.swap_in(next);
                            self.cdp.unsubscribe(&old.sid);
                            self.tab_order.retain(|t| *t != old.target);
                            switched = true;
                            let short = short_url(&self.url(), &gone);
                            self.st.lock().unwrap().events.push(format!("tab {short} closed itself; back on the previous tab"));
                        }
                    } else if let Some(i) = self.tabs.iter().position(|t| t.target == target) {
                        let t = self.tabs.remove(i);
                        self.cdp.unsubscribe(&t.sid);
                        self.tab_order.retain(|x| *x != target);
                    }
                }
            }
        }
        switched
    }

    /// One line for console.warn calls since the last report: the first one, plus a count.
    pub fn warnings_line(&self) -> Option<String> {
        let w = std::mem::take(&mut self.st.lock().unwrap().warnings);
        let first = w.first()?;
        let more = if w.len() > 1 { format!(" (+{} more: `wowsir console`)", w.len() - 1) } else { String::new() };
        Some(format!("· console.warn: {first}{more}"))
    }

    pub fn drain_events(&self) -> Vec<String> {
        std::mem::take(&mut self.st.lock().unwrap().events)
    }

    pub fn url(&self) -> String {
        self.st.lock().unwrap().url.clone()
    }

    pub async fn mouse(&self, kind: &str, x: f64, y: f64, button: &str, count: i64) -> Result<()> {
        let buttons = match (kind, button) {
            ("mousePressed", "left") => 1,
            ("mousePressed", "right") => 2,
            ("mousePressed", "middle") => 4,
            _ => 0,
        };
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": count, "modifiers": self.click_mods }),
        )
        .await?;
        Ok(())
    }

    pub async fn set_blocking(&self, types: Vec<String>) -> Result<()> {
        self.st.lock().unwrap().block_types = types;
        self.apply_fetch().await
    }

    pub async fn set_mocks(&self, mocks: Vec<Mock>) -> Result<()> {
        self.st.lock().unwrap().mocks = mocks;
        self.apply_fetch().await
    }

    /// One Fetch interception covers both `block` and `mock`.
    async fn apply_fetch(&self) -> Result<()> {
        fetch_setup(&self.cdp, &self.sid, &self.st).await?;
        // out-of-process frames make their own requests
        let frames: Vec<String> = self.st.lock().unwrap().children.iter().filter(|c| c.kind == "iframe").map(|c| c.sid.clone()).collect();
        for sid in frames {
            let _ = fetch_setup(&self.cdp, &sid, &self.st).await;
        }
        Ok(())
    }
}

pub fn exception_text(ex: &Value) -> String {
    let d = ex["exception"]["description"]
        .as_str()
        .or_else(|| ex["exception"]["value"].as_str())
        .or_else(|| ex["text"].as_str())
        .unwrap_or("error");
    let first = d.lines().next().unwrap_or(d);
    first.trim_start_matches("Uncaught ").trim_start_matches("Error: ").trim_start_matches("Retry: ").to_string()
}

fn remote_to_text(o: &Value) -> String {
    if let Some(v) = o.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(u) = o.get("unserializableValue").and_then(|v| v.as_str()) {
        return u.to_string();
    }
    if let Some(p) = o.get("preview") {
        // compact object preview: {a: 1, b: "x"}
        if let Some(props) = p.get("properties").and_then(|p| p.as_array()) {
            let arr = p.get("subtype").and_then(|s| s.as_str()) == Some("array");
            let inner: Vec<String> = props
                .iter()
                .map(|pr| {
                    let v = pr["value"].as_str().unwrap_or("");
                    let v = if pr["type"] == "string" { format!("\"{v}\"") } else { v.to_string() };
                    if arr { v } else { format!("{}: {}", pr["name"].as_str().unwrap_or(""), v) }
                })
                .collect();
            let more = if p["overflow"].as_bool() == Some(true) { ", …" } else { "" };
            return if arr { format!("[{}{}]", inner.join(", "), more) } else { format!("{{{}{}}}", inner.join(", "), more) };
        }
    }
    o.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string()
}

fn clip(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ⏎ ");
    if s.chars().count() > n { s.chars().take(n).collect::<String>() + "…" } else { s }
}

async fn event_loop(
    cdp: Arc<Cdp>,
    sid: String,
    target: String,
    st: Arc<Mutex<State>>,
    mut rx: mpsc::UnboundedReceiver<Event>,
    label: Option<Arc<Mutex<String>>>,
) {
    let child = label.is_some();
    while let Some(ev) = rx.recv().await {
        let p = &ev.params;
        // a frame is attached before it has a URL: name it once it navigates
        if let (Some(l), "Page.frameNavigated") = (&label, ev.method.as_str()) {
            if p["frame"]["id"] == target.as_str() {
                *l.lock().unwrap() = child_label("iframe", p["frame"]["url"].as_str().unwrap_or(""));
            }
        }
        let lab: Option<String> = label.as_ref().map(|l| l.lock().unwrap().clone());
        if child
            && matches!(
                ev.method.as_str(),
                "Page.frameNavigated" | "Page.navigatedWithinDocument" | "Page.frameStartedLoading" | "Page.frameStoppedLoading" | "Page.domContentEventFired"
            )
        {
            continue;
        }
        match ev.method.as_str() {
            "Page.frameAttached" if !child => {
                // any iframe may be cross-origin: lookups that miss in the page then try frames
                st.lock().unwrap().xframes = true;
            }
            "Target.attachedToTarget" => {
                tokio::spawn(attach_child(cdp.clone(), sid.clone(), st.clone(), p.clone()));
            }
            "Target.detachedFromTarget" => {
                if let Some(c) = p["sessionId"].as_str() {
                    st.lock().unwrap().children.retain(|x| x.sid != c);
                    cdp.unsubscribe(c);
                }
            }
            "Network.requestWillBeSent" => {
                let id = p["requestId"].as_str().unwrap_or("").to_string();
                let mut s = st.lock().unwrap();
                if p["type"] == "Document" && p["frameId"] == target.as_str() {
                    s.doc_status = 0;
                }
                s.inflight.insert(
                    id.clone(),
                    Req {
                        id,
                        method: p["request"]["method"].as_str().unwrap_or("GET").to_string(),
                        url: p["request"]["url"].as_str().unwrap_or("").to_string(),
                        rtype: p["type"].as_str().unwrap_or("Other").to_string(),
                        frame: p["frameId"].as_str().unwrap_or("").to_string(),
                        start: Instant::now(),
                        status: 0,
                    },
                );
                s.last_net_change = Instant::now();
            }
            "Network.responseReceived" => {
                let id = p["requestId"].as_str().unwrap_or("");
                let status = p["response"]["status"].as_i64().unwrap_or(0);
                let mut s = st.lock().unwrap();
                if p["type"] == "Document" && p["frameId"] == target.as_str() {
                    s.doc_status = status;
                }
                if let Some(r) = s.inflight.get_mut(id) {
                    r.status = status;
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                let id = p["requestId"].as_str().unwrap_or("");
                let mut s = st.lock().unwrap();
                s.last_net_change = Instant::now();
                s.delayed.remove(id);
                let Some(r) = s.inflight.remove(id) else { continue };
                let ms = r.start.elapsed().as_millis();
                let u = s.short_url(&r.url);
                if ev.method == "Network.loadingFailed" {
                    let err = match (p["errorText"].as_str().filter(|e| !e.is_empty()), p["blockedReason"].as_str(), p["corsErrorStatus"]["corsError"].as_str()) {
                        (_, Some(b), _) => format!("blocked: {b}"),
                        (_, _, Some(c)) => format!("CORS: {c}"),
                        (Some(e), _, _) => e.to_string(),
                        _ => "failed".to_string(),
                    };
                    let err = err.as_str();
                    let canceled = p["canceled"].as_bool().unwrap_or(false);
                    let blocked = err.contains("BLOCKED_BY_CLIENT") || p["blockedReason"] == "inspector";
                    s.net.push_back(format!("ERR {} {} {} ({err})", r.method, u, r.rtype));
                    if !canceled && !blocked && err != "net::ERR_ABORTED" {
                        s.note_error_in(&lab, format!("net {} {} failed: {err}", r.method, u));
                    }
                } else {
                    let line = format!("{} {} {} {} {ms}ms", r.status, r.method, u, r.rtype);
                    s.net.push_back(line.clone());
                    s.done.push_back((id.to_string(), line));
                    while s.done.len() > 300 {
                        s.done.pop_front();
                    }
                    if r.status >= 400 && !r.url.ends_with("/favicon.ico") {
                        s.note_error_in(&lab, format!("net {} {} {}", r.status, r.method, u));
                    }
                }
                while s.net.len() > 500 {
                    s.net.pop_front();
                }
            }
            "Runtime.consoleAPICalled" => {
                let ty = p["type"].as_str().unwrap_or("log");
                let args: Vec<String> =
                    p["args"].as_array().map(|a| a.iter().map(remote_to_text).collect()).unwrap_or_default();
                let text = clip(&args.join(" "), 400);
                let mut s = st.lock().unwrap();
                s.console.push_back(format!("[{ty}] {text}"));
                while s.console.len() > 500 {
                    s.console.pop_front();
                }
                if ty == "error" || ty == "assert" {
                    s.note_error_in(&lab, format!("console.error: {}", clip(&text, 300)));
                } else if ty == "warning" && s.warnings.len() < 100 {
                    let w = match &lab {
                        Some(l) => format!("{} (in {l})", clip(&text, 160)),
                        None => clip(&text, 160),
                    };
                    s.warnings.push(w);
                }
            }
            "Log.entryAdded" => {
                // Chrome's own messages: CSP violations, interventions, deprecations...
                let e = &p["entry"];
                let level = e["level"].as_str().unwrap_or("");
                let source = e["source"].as_str().unwrap_or("");
                let text = clip(e["text"].as_str().unwrap_or("").trim(), 300);
                let mut s = st.lock().unwrap();
                s.console.push_back(format!("[browser {level}] {text}"));
                // network/javascript entries duplicate what we already report from Network/Runtime
                // worker errors arrive from the worker's own session (attached), labelled
                if level == "error" && source != "network" && source != "javascript" && source != "worker" {
                    s.note_error_in(&lab, format!("browser: {text}"));
                }
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let mut msg = exception_text(d);
                if let Some(u) = d["url"].as_str().filter(|u| !u.is_empty()) {
                    let mut s = st.lock().unwrap();
                    msg = format!("{msg} ({}:{})", s.short_url(u), d["lineNumber"].as_i64().unwrap_or(0) + 1);
                    s.note_error_in(&lab, format!("uncaught {}", clip(&msg, 300)));
                    s.console.push_back(format!("[uncaught] {msg}"));
                } else {
                    let mut s = st.lock().unwrap();
                    s.note_error_in(&lab, format!("uncaught {}", clip(&msg, 300)));
                    s.console.push_back(format!("[uncaught] {msg}"));
                }
            }
            "Page.frameNavigated" => {
                let f = &p["frame"];
                if f.get("parentId").is_none() {
                    let mut s = st.lock().unwrap();
                    s.url = f["url"].as_str().unwrap_or("").to_string() + f["urlFragment"].as_str().unwrap_or("");
                    s.world = None;
                    s.doc_seq += 1;
                }
            }
            "Page.navigatedWithinDocument" => {
                if p["frameId"] == target.as_str() {
                    st.lock().unwrap().url = p["url"].as_str().unwrap_or("").to_string();
                }
            }
            "Page.frameStartedLoading" => {
                if p["frameId"] == target.as_str() {
                    st.lock().unwrap().loading = true;
                }
            }
            "Page.frameStoppedLoading" | "Page.domContentEventFired" => {
                if ev.method == "Page.domContentEventFired" || p["frameId"] == target.as_str() {
                    st.lock().unwrap().loading = false;
                }
            }
            "Runtime.executionContextsCleared" => {
                let mut s = st.lock().unwrap();
                if !child {
                    s.world = None;
                    s.xframes = false;
                }
                s.frame_worlds.retain(|_, (fs, _)| *fs != sid);
            }
            "Runtime.executionContextDestroyed" => {
                let id = p["executionContextId"].as_i64();
                let mut s = st.lock().unwrap();
                if !child && s.world.is_some() && s.world == id {
                    s.world = None;
                }
                s.frame_worlds.retain(|_, (fs, c)| !(*fs == sid && Some(*c) == id));
            }
            "Page.javascriptDialogOpening" => {
                let (accept, text) = {
                    let s = st.lock().unwrap();
                    (s.dialog_accept, s.dialog_text.clone())
                };
                let ty = p["type"].as_str().unwrap_or("alert").to_string();
                let msg = clip(p["message"].as_str().unwrap_or(""), 200);
                st.lock().unwrap().events.push(format!(
                    "dialog {ty} \"{msg}\" → {}",
                    if accept || ty == "alert" { "accepted" } else { "dismissed" }
                ));
                let prompt = match (&text, ty.as_str()) {
                    (Some(t), "prompt") => t.clone(),
                    _ => p["defaultPrompt"].as_str().unwrap_or("").to_string(),
                };
                let cdp = cdp.clone();
                let sid = sid.clone();
                tokio::spawn(async move {
                    let _ = cdp
                        .call(
                            Some(&sid),
                            "Page.handleJavaScriptDialog",
                            json!({ "accept": accept || ty == "alert", "promptText": prompt }),
                        )
                        .await;
                });
            }
            "Fetch.requestPaused" => {
                let cdp = cdp.clone();
                let sid = sid.clone();
                let id = p["requestId"].as_str().unwrap_or("").to_string();
                let url = p["request"]["url"].as_str().unwrap_or("");
                let rtype = p["resourceType"].as_str().unwrap_or("");
                let method = p["request"]["method"].as_str().unwrap_or("GET").to_string();
                let (mock, blocked) = {
                    let mut s = st.lock().unwrap();
                    let i = s.mocks.iter().position(|m| (m.doc || rtype != "Document") && glob_match(&url_glob(&m.pattern), url));
                    let mock = i.map(|i| {
                        s.mocks[i].hits += 1;
                        s.mocks[i].clone()
                    });
                    if mock.as_ref().map_or(false, |m| m.delay_ms > 0) {
                        if let Some(nid) = p["networkId"].as_str() {
                            s.delayed.insert(nid.to_string());
                        }
                    }
                    if let Some(m) = &mock {
                        let what = if m.abort { "abort".to_string() } else if m.status == 0 { format!("delay {}ms", m.delay_ms) } else { m.status.to_string() };
                        let line = format!("MOCK {what} {method} {}", s.short_url(url));
                        s.net.push_back(line);
                    }
                    (mock, s.block_types.iter().any(|t| t == rtype))
                };
                tokio::spawn(async move {
                    let (m, p) = match mock {
                        None if blocked => ("Fetch.failRequest", json!({ "requestId": id, "errorReason": "BlockedByClient" })),
                        None => ("Fetch.continueRequest", json!({ "requestId": id })),
                        Some(m) => {
                            if m.delay_ms > 0 {
                                tokio::time::sleep(Duration::from_millis(m.delay_ms)).await;
                            }
                            if m.abort {
                                ("Fetch.failRequest", json!({ "requestId": id, "errorReason": "Failed" }))
                            } else if m.status == 0 {
                                ("Fetch.continueRequest", json!({ "requestId": id }))
                            } else {
                                use base64::Engine;
                                ("Fetch.fulfillRequest", json!({
                                    "requestId": id,
                                    "responseCode": m.status,
                                    "responseHeaders": [
                                        { "name": "Content-Type", "value": m.ctype },
                                        { "name": "Access-Control-Allow-Origin", "value": "*" },
                                        { "name": "Cache-Control", "value": "no-store" },
                                    ],
                                    "body": base64::engine::general_purpose::STANDARD.encode(&m.body),
                                }))
                            }
                        }
                    };
                    let _ = cdp.call(Some(&sid), m, p).await;
                });
            }
            "Inspector.targetCrashed" => {
                st.lock().unwrap().note_error("page crashed".into());
            }
            _ => {}
        }
    }
}
