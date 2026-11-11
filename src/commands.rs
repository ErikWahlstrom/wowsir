//! Command implementations. Every command returns compact text meant to be
//! read by an LLM: one status line, then only what matters.

use crate::keys;
use crate::session::Mock;
use crate::session::Session;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Out {
    pub text: String,
    pub ok: bool,
}

impl Out {
    pub fn ok(t: impl Into<String>) -> Out {
        Out { text: t.into(), ok: true }
    }
    pub fn fail(t: impl Into<String>) -> Out {
        Out { text: t.into(), ok: false }
    }
}

/// Per-invocation options.
pub struct Ctx {
    pub cwd: PathBuf,
    /// Suppress the post-action diff (used inside `run` batches and with -q).
    pub quiet: bool,
    /// Always print a full snapshot after an action instead of a diff.
    pub full_after: bool,
    pub timeout: Option<Duration>,
}

// ----------------------------------------------------------------- arg parsing

pub struct Args {
    pub pos: Vec<String>,
    pub flags: HashMap<String, String>,
}

const VALUE_FLAGS: &[&str] =
    &["from", "grep", "g", "in", "max", "timeout", "t", "el", "out", "o", "button", "json", "body", "file", "delay", "type"];

/// Split a wowsir command line like a shell, except for `#`: `# note` (hash then
/// space) starts a comment, while `#id` is a CSS selector, not a comment.
pub fn split_line(line: &str) -> Result<Vec<String>, shell_words::ParseError> {
    let mut out = String::with_capacity(line.len() + 4);
    let (mut sq, mut dq, mut esc) = (false, false, false);
    let chars: Vec<char> = line.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if esc {
            esc = false;
        } else if c == '\\' && !sq {
            esc = true;
        } else if c == '\'' && !dq {
            sq = !sq;
        } else if c == '"' && !sq {
            dq = !dq;
        } else if c == '#' && !sq && !dq && (i == 0 || chars[i - 1].is_whitespace()) {
            if chars.get(i + 1).map_or(true, |n| n.is_whitespace()) {
                break; // `# comment`
            }
            out.push('\\'); // `#id`: keep it
        }
        out.push(c);
    }
    shell_words::split(&out)
}

/// `eval <js>` written as one line of text (a .wowsir file, `run -`, MCP): keep the
/// JavaScript exactly as written instead of shell-splitting away its quotes.
/// A script that is one fully quoted token (old style) is unquoted once.
pub fn eval_line(line: &str) -> Option<Vec<String>> {
    let (cmd, rest) = line.trim().split_once(char::is_whitespace)?;
    if cmd != "eval" && cmd != "js" {
        return None;
    }
    let rest = rest.trim();
    let quoted = rest.len() >= 2
        && ((rest.starts_with('"') && rest.ends_with('"')) || (rest.starts_with('\'') && rest.ends_with('\'')));
    let code = match shell_words::split(rest) {
        Ok(w) if quoted && w.len() == 1 => w.into_iter().next().unwrap(),
        _ => rest.to_string(),
    };
    Some(vec![cmd.to_string(), code])
}

pub fn parse_args(raw: &[String]) -> Args {
    let mut pos = vec![];
    let mut flags = HashMap::new();
    let mut i = 0;
    let mut only_pos = false;
    while i < raw.len() {
        let a = &raw[i];
        if !only_pos && a == "--" {
            only_pos = true;
        } else if !only_pos && a.starts_with('-') && a.len() > 1 && !a.starts_with("-0") && a.parse::<f64>().is_err() {
            let body = a.trim_start_matches('-');
            let (k, v) = match body.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (body.to_string(), None),
            };
            if v.is_none() && VALUE_FLAGS.contains(&k.as_str()) && i + 1 < raw.len() {
                i += 1;
                flags.insert(k, raw[i].clone());
            } else {
                flags.insert(k, v.unwrap_or_default());
            }
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    Args { pos, flags }
}

impl Args {
    fn has(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.flags.contains_key(*n))
    }
    fn get(&self, names: &[&str]) -> Option<&str> {
        names.iter().find_map(|n| self.flags.get(*n).map(|s| s.as_str()))
    }
    fn p(&self, i: usize, what: &str) -> Result<&str> {
        self.pos.get(i).map(|s| s.as_str()).ok_or_else(|| anyhow!("missing {what}"))
    }
}

// ----------------------------------------------------------------- helpers

fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

fn normalize_url(u: &str) -> String {
    if u.contains("://") || u.starts_with("about:") || u.starts_with("data:") || u.starts_with("file:") {
        return u.to_string();
    }
    if u.starts_with('/') || u.starts_with("./") {
        let p = std::fs::canonicalize(u).unwrap_or_else(|_| PathBuf::from(u));
        return format!("file://{}", p.display());
    }
    let host = u.split(['/', ':']).next().unwrap_or("");
    if host == "localhost" || host.starts_with("127.") || host == "0.0.0.0" || host.ends_with(".local") || host.ends_with(".test")
        || host.ends_with(".localhost")
    {
        format!("http://{u}")
    } else {
        format!("https://{u}")
    }
}

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// Line diff with minimal context: a changed run is preceded by its nearest
/// enclosing (less-indented) unchanged line, so the agent knows where it is.
pub fn diff(old: &[String], new: &[String]) -> (Vec<String>, usize) {
    use similar::{capture_diff_slices, Algorithm, DiffOp};
    let ops = capture_diff_slices(Algorithm::Myers, old, new);
    let mut out = vec![];
    let mut changes = 0;
    let mut last_ctx: Option<String> = None;
    // index of the nearest less-indented line above `at`
    let parent_idx = |src: &[String], at: usize| -> Option<usize> {
        let ind = indent_of(&src[at]);
        if ind == 0 {
            return None;
        }
        (0..at).rev().find(|&i| indent_of(&src[i]) < ind)
    };
    let emit = |src: &[String], range: std::ops::Range<usize>, sign: &str, out: &mut Vec<String>, last_ctx: &mut Option<String>| {
        for i in range.clone() {
            if let Some(p) = parent_idx(src, i) {
                if !range.contains(&p) {
                    let ctx = src[p].clone();
                    if last_ctx.as_ref() != Some(&ctx) {
                        out.push(format!("  {ctx}"));
                        *last_ctx = Some(ctx);
                    }
                }
            }
            out.push(format!("{sign} {}", src[i]));
        }
    };
    for op in ops {
        let (del, ins) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, .. } => (old_index..old_index + old_len, 0..0),
            DiffOp::Insert { new_index, new_len, .. } => (0..0, new_index..new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                (old_index..old_index + old_len, new_index..new_index + new_len)
            }
        };
        changes += del.len() + ins.len();
        emit(old, del, "-", &mut out, &mut last_ctx);
        emit(new, ins, "+", &mut out, &mut last_ctx);
    }
    (out, changes)
}

/// Runs of more than 3 removed lines become one "- … N lines removed" line:
/// the agent needs the new state, not a replay of what went away.
fn collapse_removed(d: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    while i < d.len() {
        if d[i].starts_with("- ") {
            let start = i;
            while i < d.len() && d[i].starts_with("- ") {
                i += 1;
            }
            if i - start > 3 {
                out.push(format!("- … {} lines removed", i - start));
            } else {
                out.extend(d[start..i].iter().cloned());
            }
        } else {
            out.push(d[i].clone());
            i += 1;
        }
    }
    out
}

fn snap_header(title: &str, url: &str) -> String {
    if title.is_empty() { format!("# {url}") } else { format!("# {title} · {url}") }
}

// ----------------------------------------------------------------- dispatcher

pub const MUTATING: &[&str] = &[
    "open", "goto", "back", "forward", "reload", "click", "dblclick", "fill", "type", "press", "select", "check",
    "uncheck", "hover", "scroll", "upload", "focus", "viewport", "resize", "theme", "tab",
];
pub const RECORDED: &[&str] = &[
    "open", "goto", "back", "forward", "reload", "click", "dblclick", "fill", "type", "press", "select", "check",
    "uncheck", "hover", "scroll", "upload", "focus", "wait", "expect", "viewport", "theme", "block", "dialog", "tab", "mock", "watch",
];

impl Session {
    pub async fn exec(&mut self, raw: &[String], ctx: &Ctx) -> Out {
        let Some(cmd) = raw.first() else { return Out::fail("no command") };
        // eval takes its script verbatim: no flag parsing (code may start with `-`)
        let args = if cmd == "eval" || cmd == "js" {
            let code: Vec<String> = raw[1..].iter().skip_while(|a| *a == "--").cloned().collect();
            Args { pos: vec![code.join(" ")].into_iter().filter(|c| !c.is_empty()).collect(), flags: HashMap::new() }
        } else {
            parse_args(&raw[1..])
        };
        let t0 = Instant::now();
        // pages the site opened/closed since the last command (e.g. a delayed popup)
        self.sync_tabs().await;
        let before_url = self.url();
        let before_doc = self.st.lock().unwrap().doc_seq;
        let mut rec: Option<Vec<String>> = None;
        if MUTATING.contains(&cmd.as_str()) {
            // timers created from here on are attributed to this action
            let _ = self.call("Runtime.evaluate", json!({ "expression": "window.__wowsir_mark && __wowsir_mark()" })).await;
        }
        // a recorded test must not depend on @eN refs (they're per page load):
        // swap them for stable selectors now, while the elements still exist
        let stable: Option<Vec<String>> = if RECORDED.contains(&cmd.as_str()) && raw[1..].iter().any(|a| Self::is_ref(a)) {
            let mut v = vec![cmd.clone()];
            for a in &raw[1..] {
                if Self::is_ref(a) {
                    match self.k("selector", &[json!(a)]).await {
                        Ok(Value::String(s)) => v.push(s),
                        _ => v.push(a.clone()),
                    }
                } else {
                    v.push(a.clone());
                }
            }
            Some(v)
        } else {
            None
        };
        let r = self.dispatch(cmd, &args, ctx, &mut rec).await;
        match r {
            Ok(head) => {
                if RECORDED.contains(&cmd.as_str()) {
                    self.history.push(shell_words::join(rec.or(stable).unwrap_or_else(|| raw.to_vec())));
                }
                if MUTATING.contains(&cmd.as_str()) {
                    self.after_action(head, t0, &before_url, before_doc, ctx).await
                } else {
                    let mut t = head;
                    if (cmd == "eval" || cmd == "js") && !ctx.quiet {
                        if let Some(c) = self.changes_since_baseline(&before_url, before_doc).await {
                            t.push_str(&c);
                        }
                        for m in self.check_watches().await {
                            t.push_str(&format!("\n! {m}"));
                        }
                    }
                    let ev = self.drain_events();
                    for e in ev {
                        t.push_str(&format!("\n! {e}"));
                    }
                    Out::ok(t)
                }
            }
            Err(e) => {
                let mut t = format!("FAIL {}: {e}", shell_words::join(raw));
                for ev in self.drain_events() {
                    t.push_str(&format!("\n! {ev}"));
                }
                let msg = e.to_string();
                if !ctx.quiet && (msg.contains("no element matches") || msg.contains("covered by") || msg.contains("stale ref")) {
                    if let Ok(s) = self.snapshot(json!({ "interactive": true, "max": 40 })).await {
                        t.push_str("\ninteractive elements now:\n");
                        t.push_str(&s.lines.join("\n"));
                        if s.more > 0 {
                            t.push_str(&format!("\n… {} more (wowsir snap -i)", s.more));
                        }
                    }
                }
                Out::fail(t)
            }
        }
    }

    /// After a mutating action: settle, then report url change, events, and
    /// a snapshot diff against what the agent last saw.
    async fn after_action(&mut self, head: String, t0: Instant, before_url: &str, before_doc: u64, ctx: &Ctx) -> Out {
        let max = if ["open", "reload", "back", "forward", "tab"].iter().any(|p| head.starts_with(p)) {
            Duration::from_secs(15)
        } else {
            Duration::from_millis(2500)
        };
        let mut note = self.settle(max).await;
        // the action opened a popup / new tab, or closed this one: follow it
        if self.sync_tabs().await {
            note = self.settle(Duration::from_secs(15)).await;
        }
        let url = self.url();
        let mut t = format!("ok {head} · {}ms", ms(t0));
        let navigated = url != before_url;
        if navigated && !head.starts_with("open") {
            t.push_str(&format!("\n→ {url}"));
        }
        let status = self.st.lock().unwrap().doc_status;
        if navigated && status >= 400 {
            t.push_str(&format!("\n! document status {status}"));
        }
        let h = head.to_lowercase();
        // signing in or out is expected to land on a login page
        let meant_to = ["login", "log in", "sign in", "signin", "logout", "log out", "sign out", "signout", "sign-out", "log-out"]
            .iter()
            .any(|k| h.contains(k));
        if navigated && looks_like_login(&url) && !looks_like_login(before_url) && !meant_to {
            // the page that bounced us is the one to sign in to (it redirects to the provider and back)
            let page = if head.starts_with("open ") { self.last_open.clone() } else { before_url.to_string() };
            t.push_str(&format!(
                "\n! redirected to a login page: not signed in or the session expired. Ask the user to sign in: you may start `wowsir login {page}` in the background (a window opens on their screen and closes itself once they are signed in), then retry"
            ));
        }
        // saved-login expiry: only for the site the agent is on
        if !self.expiry_notes.is_empty() {
            let host = crate::login::host_of(&url);
            let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.expiry_notes)
                .into_iter()
                .partition(|(n, _)| !host.is_empty() && (host == *n || host.ends_with(&format!(".{n}")) || host.contains(n.as_str())));
            self.expiry_notes = rest;
            for (_, m) in mine {
                t.push_str(&format!("\n! {m}"));
            }
        }
        let events = self.drain_events();
        // 401 from the site's own API (same-origin paths are shown as /...) = probably signed out
        let unauthorized = events.iter().any(|e| e.starts_with("net 401 ") && e.split(' ').nth(3).map_or(false, |u| u.starts_with('/')));
        for e in events {
            t.push_str(&format!("\n! {e}"));
        }
        if unauthorized && !self.warned_401 {
            self.warned_401 = true;
            let page = if head.starts_with("open ") { self.last_open.clone() } else { url.clone() };
            t.push_str(&format!(
                "\n! the site's API answered 401: probably not signed in or the session expired. Ask the user to sign in: you may start `wowsir login {page}` in the background (a window opens on their screen; they use the site's own sign-in button), then retry"
            ));
        }
        if head.starts_with("press") || head.starts_with("type") {
            // keyboard moves focus, which the diff deliberately ignores: say where it went
            if let Ok(v) = self.k("focused", &[]).await {
                if let Some(f) = v.as_str() {
                    t.push_str(&format!("\nfocus → {f}"));
                }
            }
        }
        for m in self.check_watches().await {
            t.push_str(&format!("\n! {m}"));
        }
        self.write_back_logins().await;
        if let Some(n) = note {
            t.push_str(&format!("\n… {n}"));
        }
        if ctx.quiet {
            // keep the diff base current so the next action's diff is relative to now
            if let Ok(s) = self.snapshot(json!({ "wantBase": true, "max": 0 })).await {
                if let Some(b) = s.base {
                    self.baseline = Some(b);
                }
            }
            return Out::ok(t);
        }
        match self.snapshot(json!({ "wantBase": true })).await {
            Ok(s) => {
                let base = s.base.clone().unwrap_or_default();
                let full = |t: &mut String| {
                    t.push('\n');
                    t.push_str(&snap_header(&s.title, &s.url));
                    if !s.lines.is_empty() {
                        t.push('\n');
                        t.push_str(&s.lines.join("\n"));
                    }
                    if s.more > 0 {
                        t.push_str(&format!("\n… {} more lines (wowsir snap --grep/--in/-i to focus)", s.more));
                    }
                };
                // a new document gets a full snapshot; same-document URL changes (SPA) get a diff
                let new_doc = self.st.lock().unwrap().doc_seq != before_doc;
                match (&self.baseline, new_doc || ctx.full_after) {
                    (Some(old), false) => {
                        let (d, n) = diff(old, &base);
                        if n == 0 {
                            t.push_str("\n~ no visible change");
                        } else if d.len() > 12 && d.iter().filter(|l| l.starts_with("+ ")).count() * 10 > base.len() * 8 {
                            full(&mut t); // basically a new page
                        } else {
                            t.push_str(&format!("\n~ {n} changed:\n{}", collapse_removed(&d).join("\n")));
                        }
                    }
                    _ => full(&mut t),
                }
                self.baseline = Some(base);
            }
            Err(e) => t.push_str(&format!("\n(snapshot failed: {e})")),
        }
        Out::ok(t)
    }

    /// Compare watched elements with their last known boxes, including any
    /// jump the per-frame sampler saw in between. Returns report lines.
    async fn check_watches(&mut self) -> Vec<String> {
        if self.watches.is_empty() {
            return vec![];
        }
        let list: Vec<Value> = self.watches.iter().enumerate().map(|(i, w)| json!({ "id": i, "sel": w.sel })).collect();
        let Ok(v) = self.k("watchRead", &[json!(list)]).await else { return vec![] };
        let tol = 1;
        let mut out = vec![];
        let boxv = |b: &Value| -> Option<[i64; 4]> {
            Some([b["x"].as_i64()?, b["y"].as_i64()?, b["w"].as_i64()?, b["h"].as_i64()?])
        };
        for r in v.as_array().cloned().unwrap_or_default() {
            let i = r["id"].as_u64().unwrap_or(0) as usize;
            let Some(w) = self.watches.get_mut(i) else { continue };
            if let Some(d) = r["desc"].as_str() {
                w.desc = d.to_string();
            }
            let now = boxv(&r["now"]);
            let (min, max) = (boxv(&r["min"]), boxv(&r["max"]));
            let names = ["x", "y", "w", "h"];
            let msg = match (w.base, now) {
                (Some(_), None) => Some(format!("{} is gone or hidden", w.desc)),
                (None, Some(_)) => Some(format!("{} appeared", w.desc)),
                (Some(b), Some(n)) => {
                    let moved: Vec<String> = (0..4).filter(|&k| (n[k] - b[k]).abs() > tol).map(|k| format!("{} {}→{} ({:+})", names[k], b[k], n[k], n[k] - b[k])).collect();
                    let jumped: Vec<String> = match (min, max) {
                        (Some(lo), Some(hi)) => (0..4)
                            .filter(|&k| (n[k] - b[k]).abs() <= tol && (hi[k] - lo[k]) > tol)
                            .map(|k| { let far = if (hi[k] - b[k]).abs() > (lo[k] - b[k]).abs() { hi[k] } else { lo[k] }; format!("{} {}→{}→{}", names[k], b[k], far, n[k]) })
                            .collect(),
                        _ => vec![],
                    };
                    if !moved.is_empty() {
                        Some(format!("moved: {} {}", w.desc, moved.join(", ")))
                    } else if !jumped.is_empty() {
                        Some(format!("jumped: {} {} (moved and came back)", w.desc, jumped.join(", ")))
                    } else {
                        None
                    }
                }
                (None, None) => None,
            };
            w.base = now;
            if let Some(m) = msg {
                self.moves.push((w.target.clone(), m.clone()));
                out.push(m);
            }
        }
        out
    }

    /// After `eval`: if the script changed the page, say how (and keep the diff base current).
    async fn changes_since_baseline(&mut self, before_url: &str, before_doc: u64) -> Option<String> {
        self.settle(Duration::from_millis(1500)).await;
        let s = self.snapshot(json!({ "wantBase": true, "max": 0 })).await.ok()?;
        let base = s.base?;
        let mut t = String::new();
        let url = self.url();
        if url != before_url {
            t.push_str(&format!("\n→ {url}"));
        }
        let new_doc = self.st.lock().unwrap().doc_seq != before_doc;
        if let (Some(old), false) = (&self.baseline, new_doc) {
            let (d, n) = diff(old, &base);
            if n > 0 {
                t.push_str(&format!("\n~ {n} changed:\n{}", collapse_removed(&d).join("\n")));
            }
        } else if new_doc {
            t.push_str("\n~ new page loaded (wowsir snap to see it)");
        }
        self.baseline = Some(base);
        (!t.is_empty()).then_some(t)
    }

    /// Load a storageState file (cookies + localStorage) into this session.
    pub async fn load_state(&mut self, p: &Path) -> Result<(usize, usize)> {
        let st: Value = serde_json::from_str(&std::fs::read_to_string(p).map_err(|e| anyhow!("{}: {e}", p.display()))?)?;
        let cookies: Vec<Value> = st["cookies"].as_array().map(|a| a.iter().map(crate::login::clean_cookie).collect()).unwrap_or_default();
        if !cookies.is_empty() {
            self.call("Storage.setCookies", json!({ "cookies": cookies, "browserContextId": self.browser_ctx })).await?;
        }
        let mut n_ls = 0;
        for o in st["origins"].as_array().cloned().unwrap_or_default() {
            let origin = o["origin"].as_str().unwrap_or("");
            n_ls += 1;
            let src = format!(
                "if (location.origin === {} && !sessionStorage.getItem('__wowsir_ls')) {{ for (const i of {}) localStorage.setItem(i.name, i.value); for (const i of {}) sessionStorage.setItem(i.name, i.value); sessionStorage.setItem('__wowsir_ls','1'); }}",
                json!(origin),
                if o["localStorage"].is_array() { o["localStorage"].clone() } else { json!([]) },
                if o["sessionStorage"].is_array() { o["sessionStorage"].clone() } else { json!([]) }
            );
            self.call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": src })).await?;
        }
        Ok((cookies.len(), n_ls))
    }

    /// Remove a site's cookies (by domain) and web storage (by origin) from this session.
    pub async fn forget_site(&mut self, domains: &[String], origins: &[String]) -> usize {
        let mut n = 0;
        if let Ok(r) = self.call("Storage.getCookies", json!({ "browserContextId": self.browser_ctx })).await {
            for c in r["cookies"].as_array().cloned().unwrap_or_default() {
                let d = c["domain"].as_str().unwrap_or("");
                if domains.iter().any(|x| x == d) {
                    let _ = self
                        .call("Network.deleteCookies", json!({ "name": c["name"], "domain": d, "path": c["path"] }))
                        .await;
                    n += 1;
                }
            }
        }
        for o in origins {
            let _ = self
                .call("Storage.clearDataForOrigin", json!({ "origin": o, "storageTypes": "local_storage,indexeddb,service_workers,cache_storage" }))
                .await;
        }
        // sessionStorage lives in the tab: clear it if we're on one of those origins
        let _ = self
            .js(&format!("{}.includes(location.origin) ? (sessionStorage.clear(), true) : false", json!(origins)))
            .await;
        n
    }

    /// Sites rotate/extend session cookies while you use them. Write fresher
    /// cookies back to the saved login so it lives as long as the server allows.
    pub async fn write_back_logins(&mut self) {
        if self.last_writeback.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.last_writeback = Instant::now();
        let Ok(r) = self.call("Storage.getCookies", json!({ "browserContextId": self.browser_ctx })).await else { return };
        let live: Vec<Value> = r["cookies"].as_array().map(|a| a.iter().map(crate::login::clean_cookie).collect()).unwrap_or_default();
        let Ok(rd) = std::fs::read_dir(crate::login::auth_dir()) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map_or(true, |x| x != "json") {
                continue;
            }
            let Ok(mut st) = std::fs::read_to_string(&p).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str::<Value>(&t)?)) else { continue };
            let Some(saved) = st["cookies"].as_array_mut() else { continue };
            let mut changed = false;
            for c in saved.iter_mut() {
                let fresh = live.iter().find(|l| l["name"] == c["name"] && l["domain"] == c["domain"] && l["path"] == c["path"]);
                if let Some(f) = fresh {
                    let newer = f["expires"].as_f64().unwrap_or(-1.0) > c["expires"].as_f64().unwrap_or(-1.0);
                    if f["value"] != c["value"] || newer {
                        *c = f.clone();
                        changed = true;
                    }
                }
            }
            if changed {
                let _ = crate::login::write_private(&p, &serde_json::to_string_pretty(&st).unwrap_or_default());
            }
        }
    }

    /// Load every saved login in the auth dir (~/.wowsir/auth/*.json).
    pub async fn load_auth_dir(&mut self) -> Vec<String> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        for (name, exp, login_url) in crate::login::saved_logins() {
            let Some(exp) = exp else { continue };
            let how = login_url.map(|u| format!("`wowsir login {u}`")).unwrap_or_else(|| "`wowsir login <url>`".into());
            self.expiry_notes.retain(|(n, _)| *n != name);
            if exp < now {
                self.expiry_notes.push((name.clone(), format!("saved login '{name}' has expired: ask the user to sign in again ({how})")));
            } else if exp - now < 3600.0 {
                self.expiry_notes.push((name.clone(), format!("saved login '{name}' expires in {} min: consider asking the user to sign in again ({how})", ((exp - now) / 60.0) as i64)));
            }
        }
        let mut loaded = vec![];
        let Ok(rd) = std::fs::read_dir(crate::login::auth_dir()) else { return loaded };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "json") {
                if let Ok((c, _)) = self.load_state(&p).await {
                    loaded.push(format!("{} ({c} cookies)", p.file_stem().unwrap_or_default().to_string_lossy()));
                }
            }
        }
        loaded
    }

    fn tmo(&self, ctx: &Ctx, args: &Args) -> Duration {
        args.get(&["timeout", "t"])
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .or(ctx.timeout)
            .unwrap_or(self.timeout)
    }

    /// Poll a `__wowsir.<f>` action until it stops returning {retry}.
    async fn actionable(&mut self, f: &str, args: &[Value], timeout: Duration) -> Result<Value> {
        let start = Instant::now();
        loop {
            let v = self.k(f, args).await?;
            match v.get("retry").and_then(|r| r.as_str()) {
                None => return Ok(v),
                Some(reason) => {
                    if start.elapsed() >= timeout {
                        bail!("{reason} (waited {}ms)", timeout.as_millis());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    /// Resolve target → move mouse there → re-measure (hover can shift layout)
    /// → press. Returns the point() result.
    async fn click_target(&mut self, target: &str, button: &str, count: i64, to: Duration) -> Result<Value> {
        let mut v = self.actionable("point", &[json!(target), json!(Self::is_ref(target))], to).await?;
        let (mut x, mut y) = (v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0));
        self.mouse("mouseMoved", x, y, "none", 0).await?;
        for _ in 0..3 {
            let v2 = self.actionable("point", &[json!(target), json!(false)], to).await?;
            let (x2, y2) = (v2["x"].as_f64().unwrap_or(0.0), v2["y"].as_f64().unwrap_or(0.0));
            if (x2 - x).abs() < 1.0 && (y2 - y).abs() < 1.0 {
                break;
            }
            x = x2;
            y = y2;
            self.mouse("mouseMoved", x, y, "none", 0).await?;
        }
        for i in 1..=count {
            self.mouse("mousePressed", x, y, button, i).await?;
            self.mouse("mouseReleased", x, y, button, i).await?;
        }
        v["x"] = json!(x);
        v["y"] = json!(y);
        Ok(v)
    }

    fn is_ref(t: &str) -> bool {
        let t = t.strip_prefix('@').unwrap_or(t);
        t.starts_with('e') && t[1..].chars().all(|c| c.is_ascii_digit()) && t.len() > 1
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        for c in text.chars() {
            let k = keys::char_key(c);
            let mut down = json!({"type":"keyDown","key":k.key,"code":k.code,"windowsVirtualKeyCode":k.vk,"text":k.text,"unmodifiedText":k.text});
            if k.vk == 0 {
                down.as_object_mut().unwrap().remove("windowsVirtualKeyCode");
            }
            self.call("Input.dispatchKeyEvent", down).await?;
            self.call("Input.dispatchKeyEvent", json!({"type":"keyUp","key":k.key,"code":k.code,"windowsVirtualKeyCode":k.vk}))
                .await?;
        }
        Ok(())
    }

    async fn dispatch(&mut self, cmd: &str, a: &Args, ctx: &Ctx, rec: &mut Option<Vec<String>>) -> Result<String> {
        let to = self.tmo(ctx, a);
        match cmd {
            "open" | "goto" | "navigate" => {
                let url = normalize_url(a.p(0, "url")?);
                self.last_open = url.clone();
                self.warned_401 = false;
                {
                    let mut s = self.st.lock().unwrap();
                    s.errors.clear();
                    s.events.clear();
                    s.loading = true;
                    s.world = None;
                }
                self.baseline = None;
                let r = self.call("Page.navigate", json!({ "url": url })).await?;
                if let Some(e) = r.get("errorText").and_then(|e| e.as_str()).filter(|e| !e.is_empty()) {
                    self.st.lock().unwrap().loading = false;
                    bail!("navigation failed: {e}");
                }
                if r.get("loaderId").is_none() {
                    // same document, only the #hash changed: the page was not reloaded
                    self.st.lock().unwrap().loading = false;
                    return Ok(format!("open {url} (same page, only the #hash changed: no reload; `wowsir reload` for a fresh load)"));
                }
                let status = self.st.lock().unwrap().doc_status;
                Ok(if status > 0 && status != 200 { format!("open {url} ({status})") } else { format!("open {url}") })
            }
            "back" | "forward" => {
                let h = self.call("Page.getNavigationHistory", json!({})).await?;
                let cur = h["currentIndex"].as_i64().unwrap_or(0);
                let idx = if cmd == "back" { cur - 1 } else { cur + 1 };
                let entries = h["entries"].as_array().cloned().unwrap_or_default();
                let e = entries.get(idx as usize).filter(|_| idx >= 0).ok_or_else(|| anyhow!("no history entry to go {cmd} to"))?;
                self.st.lock().unwrap().loading = true;
                self.call("Page.navigateToHistoryEntry", json!({ "entryId": e["id"] })).await?;
                Ok(cmd.to_string())
            }
            "tabs" => {
                let mut out = vec![];
                for (i, id) in self.tab_order.iter().enumerate() {
                    let (active, url) = if *id == self.target {
                        (true, self.url())
                    } else {
                        let u = self.tabs.iter().find(|t| t.target == *id).map(|t| t.st.lock().unwrap().url.clone()).unwrap_or_default();
                        (false, u)
                    };
                    out.push(format!("{} {i} {url}", if active { "*" } else { " " }));
                }
                Ok(out.join("\n"))
            }
            "tab" => {
                let which = a.p(0, "tab number, `new [URL]` or `close`")?;
                match which {
                    "new" => {
                        self.new_tab().await?;
                        if let Some(u) = a.pos.get(1) {
                            let url = normalize_url(u);
                            self.last_open = url.clone();
                            self.st.lock().unwrap().loading = true;
                            self.call("Page.navigate", json!({ "url": url })).await?;
                        }
                        Ok(format!("tab new → tab {}", self.tab_order.len() - 1))
                    }
                    "close" => {
                        if let Some(n) = a.pos.get(1) {
                            let i: usize = n.parse().map_err(|_| anyhow!("tab close takes a tab number"))?;
                            let id = self.tab_order.get(i).cloned().ok_or_else(|| anyhow!("no tab {i} (wowsir tabs lists them)"))?;
                            if id != self.target {
                                let t = self.tabs.iter().position(|t| t.target == id).map(|p| self.tabs.remove(p));
                                if let Some(t) = t {
                                    self.cdp.unsubscribe(&t.sid);
                                }
                                self.tab_order.retain(|x| *x != id);
                                let _ = self.cdp.call(None, "Target.closeTarget", json!({ "targetId": id })).await;
                                return Ok(format!("tab close {i}"));
                            }
                        }
                        self.close_active().await?;
                        let i = self.tab_order.iter().position(|t| *t == self.target).unwrap_or(0);
                        Ok(format!("tab close → tab {i}"))
                    }
                    n => {
                        let i: usize = n.parse().map_err(|_| anyhow!("usage: tab N | tab new [URL] | tab close [N]"))?;
                        let id = self.tab_order.get(i).cloned().ok_or_else(|| anyhow!("no tab {i} (wowsir tabs lists them)"))?;
                        if id != self.target {
                            self.activate(&id);
                        }
                        let _ = self.call("Page.bringToFront", json!({})).await;
                        Ok(format!("tab {i}"))
                    }
                }
            }
            "reload" => {
                self.st.lock().unwrap().loading = true;
                self.call("Page.reload", json!({})).await?;
                Ok("reload".into())
            }
            "snap" | "snapshot" | "s" => {
                let scoped = a.has(&["in", "viewport", "v"]);
                let opts = json!({
                    "interactive": a.has(&["i", "interactive"]),
                    "full": a.has(&["full", "f"]),
                    "grep": a.get(&["grep", "g"]),
                    "scope": a.get(&["in"]),
                    "viewport": a.has(&["viewport", "v"]),
                    "urls": a.has(&["urls", "u"]),
                    "ids": a.has(&["ids"]),
                    "max": a.get(&["max"]).and_then(|m| m.parse::<u64>().ok()).unwrap_or(if a.has(&["all"]) { 100000 } else { 400 }),
                    "wantBase": !scoped,
                });
                let s = self.snapshot(opts).await?;
                if let Some(b) = s.base.clone() {
                    self.baseline = Some(b);
                }
                let mut t = snap_header(&s.title, &s.url);
                if s.lines.is_empty() {
                    t.push_str(if a.has(&["grep", "g"]) { "\n(no matching lines)" } else { "\n(empty)" });
                } else {
                    t.push('\n');
                    t.push_str(&s.lines.join("\n"));
                }
                if s.more > 0 {
                    t.push_str(&format!("\n… {} more lines (use --grep, --in <target>, -i, or --all)", s.more));
                }
                Ok(t)
            }
            "click" | "dblclick" => {
                let target = a.p(0, "target")?;
                let count = if cmd == "dblclick" || a.has(&["double"]) { 2 } else { 1 };
                let button = if a.has(&["right"]) { "right" } else { a.get(&["button"]).unwrap_or("left") };
                // --ctrl / --shift / --alt / --meta / --mod (Meta on macOS, else Ctrl): multi-select, open-in-new-tab...
                let mod_key = if cfg!(target_os = "macos") { 4 } else { 2 };
                let flags = [("alt", 1), ("ctrl", 2), ("meta", 4), ("shift", 8), ("mod", mod_key)];
                let held: Vec<&str> = flags.iter().filter(|(f, _)| a.has(&[f])).map(|(f, _)| *f).collect();
                self.click_mods = flags.iter().filter(|(f, _)| a.has(&[f])).fold(0, |m, (_, b)| m | b);
                let v = self.click_target(target, button, count, to).await;
                self.click_mods = 0;
                let v = v?;
                if let Some(sel) = v["sel"].as_str() {
                    let mut r = vec![cmd.to_string(), sel.to_string()];
                    if button != "left" {
                        r.push("--right".into());
                    }
                    r.extend(held.iter().map(|f| format!("--{f}")));
                    *rec = Some(r);
                }
                let with = if held.is_empty() { String::new() } else { format!(" with {}", held.join("+")) };
                Ok(format!("{cmd} {}{with}", v["desc"].as_str().unwrap_or(target)))
            }
            "hover" => {
                let target = a.p(0, "target")?;
                let v = self.actionable("point", &[json!(target), json!(Self::is_ref(target))], to).await?;
                self.mouse("mouseMoved", v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0), "none", 0).await?;
                if let Some(sel) = v["sel"].as_str() {
                    *rec = Some(vec!["hover".into(), sel.into()]);
                }
                Ok(format!("hover {}", v["desc"].as_str().unwrap_or(target)))
            }
            "fill" => {
                let target = a.p(0, "target")?;
                let text = a.pos[1..].join(" ");
                let v = self.actionable("prepFill", &[json!(target), json!(Self::is_ref(target))], to).await?;
                if v["direct"].as_bool() == Some(true) {
                    self.k("setValue", &[json!(target), json!(text)]).await?;
                } else if text.is_empty() {
                    if v["empty"].as_bool() != Some(true) {
                        for e in keys::chord("Backspace").map_err(|e| anyhow!(e))? {
                            self.call("Input.dispatchKeyEvent", e).await?;
                        }
                    }
                } else {
                    self.call("Input.insertText", json!({ "text": text })).await?;
                }
                if let Some(sel) = v["sel"].as_str() {
                    *rec = Some(vec!["fill".into(), sel.into(), text.clone()]);
                }
                Ok(format!("fill {} = \"{}\"", v["desc"].as_str().unwrap_or(target), text))
            }
            "type" => {
                // type <text>  |  type <target> <text>
                let (target, text) = if a.pos.len() >= 2 { (Some(a.pos[0].clone()), a.pos[1..].join(" ")) } else { (None, a.p(0, "text")?.to_string()) };
                let mut desc = "focused element".to_string();
                if let Some(t) = &target {
                    let v = self.click_target(t, "left", 1, to).await?;
                    desc = v["desc"].as_str().unwrap_or(t).to_string();
                    if let Some(sel) = v["sel"].as_str() {
                        *rec = Some(vec!["type".into(), sel.into(), text.clone()]);
                    }
                }
                self.type_text(&text).await?;
                Ok(format!("type \"{text}\" into {desc}"))
            }
            "press" | "key" => {
                if a.pos.is_empty() {
                    bail!("missing key, e.g. `wowsir press Enter` or `wowsir press Control+a`");
                }
                for spec in &a.pos {
                    for e in keys::chord(spec).map_err(|e| anyhow!(e))? {
                        self.call("Input.dispatchKeyEvent", e).await?;
                    }
                }
                Ok(format!("press {}", a.pos.join(" ")))
            }
            "select" => {
                let target = a.p(0, "target")?;
                let values: Vec<String> = a.pos[1..].to_vec();
                if values.is_empty() {
                    bail!("missing option value/label");
                }
                let v = self.actionable("select", &[json!(target), json!(values), json!(Self::is_ref(target))], to).await?;
                if let Some(sel) = v["sel"].as_str() {
                    let mut r = vec!["select".to_string(), sel.to_string()];
                    r.extend(values.iter().cloned());
                    *rec = Some(r);
                }
                Ok(format!("select {} = {}", v["desc"].as_str().unwrap_or(target), v["picked"]))
            }
            "check" | "uncheck" => {
                let target = a.p(0, "target")?;
                let want = cmd == "check";
                let cur = self.k("checked", &[json!(target)]).await?;
                if cur.as_bool() == Some(want) {
                    let d = self.k("describe", &[json!(target)]).await?;
                    return Ok(format!("{cmd} {} (already)", d.as_str().unwrap_or(target)));
                }
                // tri-state boxes cycle through mixed: keep clicking (max 3) until the wanted state
                let mut path = vec![tri(&cur)];
                let mut desc = target.to_string();
                for i in 0..3 {
                    let v = self.click_target(target, "left", 1, to).await?;
                    if i == 0 {
                        if let Some(sel) = v["sel"].as_str() {
                            *rec = Some(vec![cmd.into(), sel.into()]);
                        }
                    }
                    desc = v["desc"].as_str().unwrap_or(target).to_string();
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    let now = self.k("checked", &[json!(target)]).await.unwrap_or(Value::Null);
                    path.push(tri(&now));
                    if now.as_bool() == Some(want) {
                        return Ok(if i == 0 { format!("{cmd} {desc}") } else { format!("{cmd} {desc} ({})", path.join(" → ")) });
                    }
                    if now == cur && i == 0 {
                        bail!("clicked {desc} but its checked state did not change");
                    }
                }
                bail!("clicked {desc} 3 times but never reached {}: {}", if want { "checked" } else { "unchecked" }, path.join(" → "))
            }
            "focus" => {
                let target = a.p(0, "target")?;
                self.js(&format!("__wowsir.resolve({}).focus()", json!(target))).await?;
                Ok(format!("focus {target}"))
            }
            "scroll" => {
                let what = a.pos.first().map(|s| s.as_str()).unwrap_or("down");
                let amount: Option<f64> = a.pos.get(1).and_then(|p| p.parse().ok());
                match what {
                    "down" | "up" => {
                        let info = self.k("scrollInfo", &[]).await?;
                        let vh = info["vh"].as_f64().unwrap_or(800.0);
                        let dy = amount.unwrap_or(vh * 0.8) * if what == "up" { -1.0 } else { 1.0 };
                        if info["h"].as_f64().unwrap_or(0.0) > vh + 1.0 {
                            // the page itself scrolls: a wheel at the center could land on a map/canvas and zoom it
                            self.js(&format!("scrollBy({{top:{dy},behavior:'instant'}})")).await?;
                        } else {
                            // app shell with an inner scroller: wheel where the content is
                            self.call(
                                "Input.dispatchMouseEvent",
                                json!({"type":"mouseWheel","x":640,"y":vh/2.0,"deltaX":0,"deltaY":dy}),
                            )
                            .await?;
                            tokio::time::sleep(Duration::from_millis(30)).await;
                        }
                        let after = self.k("scrollInfo", &[]).await?;
                        Ok(format!("scroll {what} → y={} of {}", after["y"], after["h"]))
                    }
                    "top" => {
                        self.js("scrollTo({top:0,behavior:'instant'})").await?;
                        Ok("scroll top".into())
                    }
                    "bottom" => {
                        self.js("scrollTo({top:document.documentElement.scrollHeight,behavior:'instant'})").await?;
                        Ok("scroll bottom".into())
                    }
                    t if a.pos.len() > 1 => {
                        // scroll inside the element: `scroll T down|up [PX]` or `scroll T PX`
                        let (dir, amt) = match a.pos[1].as_str() {
                            d @ ("down" | "up") => (d, a.pos.get(2).and_then(|p| p.parse::<f64>().ok())),
                            n => {
                                let px: f64 = n.parse().map_err(|_| anyhow!("scroll T takes down|up [PX] or PX, got {n}"))?;
                                if px < 0.0 { ("up", Some(-px)) } else { ("down", Some(px)) }
                            }
                        };
                        let v = self.k("scrollIn", &[json!(t), json!(amt), json!(dir)]).await?;
                        Ok(format!("scroll {} {dir} → y={} of {}", v["desc"].as_str().unwrap_or(t), v["y"], v["h"]))
                    }
                    t => {
                        let d = self
                            .js(&format!(
                                "(()=>{{const e=__wowsir.resolve({});e.scrollIntoView({{block:'center',behavior:'instant'}});return __wowsir.describe(e)}})()",
                                json!(t)
                            ))
                            .await?;
                        Ok(format!("scroll to {}", d.as_str().unwrap_or(t)))
                    }
                }
            }
            "upload" => {
                let target = a.p(0, "target")?;
                if a.pos.len() < 2 {
                    bail!("missing file path(s)");
                }
                let files: Vec<String> = a.pos[1..]
                    .iter()
                    .map(|f| {
                        let p = Path::new(f);
                        let p = if p.is_absolute() { p.to_path_buf() } else { ctx.cwd.join(p) };
                        p.display().to_string()
                    })
                    .collect();
                for f in &files {
                    if !Path::new(f).exists() {
                        bail!("file not found: {f}");
                    }
                }
                let obj = self.js_object(&format!("__wowsir.isFileInput({})", json!(target))).await?;
                self.call("DOM.setFileInputFiles", json!({ "files": files, "objectId": obj })).await?;
                Ok(format!("upload {} file(s) to {target}", files.len()))
            }
            "wait" => self.cmd_wait(a, to).await,
            "expect" | "assert" => self.cmd_expect(a, to).await,
            "text" => {
                let max: usize = a.get(&["max"]).and_then(|m| m.parse().ok()).unwrap_or(8000);
                let v = self.k("text", &[a.pos.first().map(|s| json!(s)).unwrap_or(Value::Null)]).await?;
                let s = v.as_str().unwrap_or("");
                Ok(truncate_chars(s, max))
            }
            "html" => {
                let max: usize = a.get(&["max"]).and_then(|m| m.parse().ok()).unwrap_or(8000);
                let v = self.k("html", &[a.pos.first().map(|s| json!(s)).unwrap_or(Value::Null)]).await?;
                Ok(truncate_chars(v.as_str().unwrap_or(""), max))
            }
            "eval" | "js" => {
                let code = a.pos.join(" ");
                if code.is_empty() {
                    bail!("missing JavaScript. Pass it quoted (wowsir eval 'document.title'), from a file (wowsir eval --file check.js) or stdin (wowsir eval -)");
                }
                // main world: page globals are visible here
                let r = self
                    .call(
                        "Runtime.evaluate",
                        json!({ "expression": code, "returnByValue": false, "awaitPromise": true, "replMode": true }),
                    )
                    .await?;
                if let Some(ex) = r.get("exceptionDetails") {
                    bail!("{}", crate::session::exception_text(ex));
                }
                let mut v = r["result"].clone();
                // objects come back by handle: await promises (replMode doesn't), then serialize
                if let (Some(id), false) = (v["objectId"].as_str(), v["subtype"] == "node") {
                    let r = self
                        .call(
                            "Runtime.callFunctionOn",
                            json!({ "objectId": id, "functionDeclaration": "async function () { return await this; }", "returnByValue": true, "awaitPromise": true }),
                        )
                        .await?;
                    if let Some(ex) = r.get("exceptionDetails") {
                        bail!("{}", crate::session::exception_text(ex));
                    }
                    v = r["result"].clone();
                }
                Ok(match v.get("value") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v)?,
                    None => v["description"].as_str().unwrap_or("undefined").to_string(),
                })
            }
            "watch" => {
                let first = a.pos.first().map(|s| s.as_str());
                if first.is_none() {
                    if self.watches.is_empty() {
                        return Ok("not watching anything (wowsir watch T... to start)".into());
                    }
                    self.check_watches().await;
                    let lines: Vec<String> = self
                        .watches
                        .iter()
                        .map(|w| match w.base {
                            Some(b) => format!("{} at {},{} {}×{}", w.desc, b[0], b[1], b[2], b[3]),
                            None => format!("{} (not on the page)", w.desc),
                        })
                        .collect();
                    return Ok(lines.join("\n"));
                }
                if matches!(first, Some("off" | "clear" | "stop")) {
                    match a.pos.get(1) {
                        Some(t) => self.watches.retain(|w| w.target != *t),
                        None => self.watches.clear(),
                    }
                    self.moves.clear();
                    self.check_watches().await;
                    return Ok(format!("watching {} element(s)", self.watches.len()));
                }
                let mut added = vec![];
                for t in &a.pos {
                    let sel = self.k("selector", &[json!(t)]).await?;
                    let sel = if Self::is_ref(t) { sel.as_str().unwrap_or(t).to_string() } else { t.clone() };
                    self.watches.retain(|w| w.target != *t);
                    self.watches.push(crate::session::Watch { target: t.clone(), sel, desc: t.clone(), base: None });
                    added.push(t.clone());
                }
                self.check_watches().await; // takes the initial boxes
                self.moves.clear();
                *rec = Some(std::iter::once("watch".to_string()).chain(self.watches.iter().filter(|w| added.contains(&w.target)).map(|w| w.sel.clone())).collect());
                let lines: Vec<String> = self
                    .watches
                    .iter()
                    .filter(|w| added.contains(&w.target))
                    .map(|w| match w.base {
                        Some(b) => format!("watch {} at {},{} {}×{}", w.desc, b[0], b[1], b[2], b[3]),
                        None => format!("watch {} (not visible now)", w.desc),
                    })
                    .collect();
                Ok(lines.join("\n") + "\n(each action now reports `! moved:` or `! jumped:` for these; `expect still` asserts none did)")
            }
            "screenshot" | "shot" => {
                let full = a.has(&["full", "f"]);
                let marking = a.has(&["mark"]);
                if marking {
                    let mut targets: Vec<String> = a.get(&["mark"]).filter(|m| !m.is_empty()).map(|m| vec![m.to_string()]).unwrap_or_default();
                    if targets.is_empty() {
                        targets = self.watches.iter().map(|w| w.sel.clone()).collect();
                    }
                    if targets.is_empty() {
                        bail!("--mark outlines watched elements (wowsir watch T... first), or name one with --mark=T");
                    }
                    self.k("mark", &[json!(targets)]).await?;
                }
                let path = a.pos.first().map(|p| {
                    let p = Path::new(p);
                    if p.is_absolute() { p.to_path_buf() } else { ctx.cwd.join(p) }
                });
                let path = path.unwrap_or_else(|| {
                    std::env::temp_dir().join(format!("wowsir-{}-{}.png", self.name, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()))
                });
                let jpeg = path.extension().map(|e| e == "jpg" || e == "jpeg").unwrap_or(false);
                let mut p = json!({ "format": if jpeg { "jpeg" } else { "png" }, "optimizeForSpeed": true });
                if jpeg {
                    p["quality"] = json!(80);
                }
                if let Some(el) = a.get(&["el", "in"]) {
                    let r = self.k("rect", &[json!(el)]).await?;
                    p["clip"] = json!({"x": r["x"], "y": r["y"], "width": r["w"], "height": r["h"], "scale": 1});
                    p["captureBeyondViewport"] = json!(true);
                } else if full {
                    let m = self.call("Page.getLayoutMetrics", json!({})).await?;
                    let cs = &m["cssContentSize"];
                    p["clip"] = json!({"x":0,"y":0,"width":cs["width"],"height":cs["height"],"scale":1});
                    p["captureBeyondViewport"] = json!(true);
                }
                let r = self.call("Page.captureScreenshot", p).await;
                if marking {
                    let _ = self.k("unmark", &[]).await;
                }
                let r = r?;
                let bytes = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or(""))?;
                std::fs::write(&path, &bytes)?;
                Ok(format!("{} ({} KB)", path.display(), bytes.len() / 1024))
            }
            "console" | "logs" => {
                let mut s = self.st.lock().unwrap();
                if a.has(&["clear"]) {
                    s.console.clear();
                    return Ok("console cleared".into());
                }
                let n = a.get(&["max"]).and_then(|m| m.parse().ok()).unwrap_or(if a.has(&["all"]) { 500 } else { 40 });
                let errs = a.has(&["errors", "e"]);
                let lines: Vec<&String> = s.console.iter().filter(|l| !errs || l.starts_with("[error]") || l.starts_with("[uncaught]")).collect();
                let skip = lines.len().saturating_sub(n);
                let out: Vec<String> = lines.into_iter().skip(skip).cloned().collect();
                Ok(if out.is_empty() { "(no console output)".into() } else { out.join("\n") })
            }
            "net" | "network" if a.get(&["body"]).is_some() => {
                let pat = a.get(&["body"]).unwrap_or("").to_lowercase();
                let parts: Vec<&str> = pat.split('|').map(str::trim).filter(|p| !p.is_empty()).collect();
                let hit = {
                    let s = self.st.lock().unwrap();
                    s.done.iter().rev().find(|(_, l)| { let l = l.to_lowercase(); parts.iter().any(|p| l.contains(p)) }).cloned()
                };
                let (id, line) = hit.ok_or_else(|| anyhow!("no completed response matches '{pat}' (wowsir net lists requests; streams show up once they finish)"))?;
                let r = self.call("Network.getResponseBody", json!({ "requestId": id })).await.map_err(|e| anyhow!("{line}: body not available ({e})"))?;
                let raw = r["body"].as_str().unwrap_or("");
                let body = if r["base64Encoded"].as_bool() == Some(true) {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).unwrap_or_default();
                    match String::from_utf8(bytes) {
                        Ok(s) => s,
                        Err(e) => format!("({} bytes of binary data)", e.into_bytes().len()),
                    }
                } else {
                    raw.to_string()
                };
                let max = a.get(&["max"]).and_then(|m| m.parse::<usize>().ok()).unwrap_or(if a.has(&["full", "all"]) { usize::MAX } else { 4000 });
                let n = body.chars().count();
                let shown: String = body.chars().take(max).collect();
                let more = if n > max { format!("\n… {} more chars (--full or --max N)", n - max) } else { String::new() };
                Ok(format!("{line}\n{shown}{more}"))
            }
            "net" | "network" => {
                let s = self.st.lock().unwrap();
                let failed = a.has(&["failed", "errors", "e"]);
                let grep = a.get(&["grep", "g"]).map(|g| g.to_lowercase());
                let api = a.has(&["api", "xhr"]);
                let n = a.get(&["max"]).and_then(|m| m.parse().ok()).unwrap_or(if a.has(&["all"]) { 500 } else { 40 });
                let lines: Vec<&String> = s
                    .net
                    .iter()
                    .filter(|l| !failed || l.starts_with("ERR") || l.starts_with('4') || l.starts_with('5'))
                    .filter(|l| !api || l.contains(" XHR ") || l.contains(" Fetch ") || l.contains(" Document "))
                    .filter(|l| grep.as_ref().map_or(true, |g| { let l = l.to_lowercase(); g.split('|').map(str::trim).filter(|p| !p.is_empty()).any(|p| l.contains(p)) }))
                    .collect();
                let skip = lines.len().saturating_sub(n);
                let out: Vec<String> = lines.into_iter().skip(skip).cloned().collect();
                let pending = s.inflight.len();
                let mut t = if out.is_empty() { "(no matching requests)".to_string() } else { out.join("\n") };
                if pending > 0 {
                    t.push_str(&format!("\n({pending} in flight)"));
                }
                Ok(t)
            }
            "errors" => {
                let mut s = self.st.lock().unwrap();
                if a.has(&["clear"]) {
                    s.errors.clear();
                    return Ok("errors cleared".into());
                }
                Ok(if s.errors.is_empty() { "no errors".into() } else { s.errors.join("\n") })
            }
            "viewport" | "resize" => {
                let preset = a.pos.first().map(|s| s.as_str()).unwrap_or("");
                let (w, h, mobile) = match preset {
                    "mobile" | "iphone" => (390, 844, true),
                    "tablet" | "ipad" => (820, 1180, true),
                    "desktop" => (1280, 800, false),
                    "hd" => (1920, 1080, false),
                    s if s.contains('x') => {
                        let (w, h) = s.split_once('x').unwrap();
                        let bad = || anyhow!("viewport takes W H, WxH or mobile|tablet|desktop|hd");
                        (w.parse::<u32>().map_err(|_| bad())?, h.parse::<u32>().map_err(|_| bad())?, a.has(&["mobile", "m"]))
                    }
                    _ => (
                        a.p(0, "width")?.parse::<u32>()?,
                        a.p(1, "height")?.parse::<u32>()?,
                        a.has(&["mobile", "m"]),
                    ),
                };
                self.call(
                    "Emulation.setDeviceMetricsOverride",
                    json!({"width": w, "height": h, "deviceScaleFactor": if mobile {3} else {1}, "mobile": mobile}),
                )
                .await?;
                self.call("Emulation.setTouchEmulationEnabled", json!({ "enabled": mobile })).await?;
                // phones also differ in user agent, which server-side mobile detection reads
                let ver = self.call("Browser.getVersion", json!({})).await.ok()
                    .and_then(|v| v["product"].as_str().and_then(|p| p.split('/').nth(1)).map(|v| v.split('.').next().unwrap_or("0").to_string()))
                    .unwrap_or_else(|| "131".into());
                let ua = if mobile {
                    let tok = if w >= 600 { "" } else { " Mobile" };
                    json!({"userAgent": format!("Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{ver}.0.0.0{tok} Safari/537.36"),
                           "userAgentMetadata": {"platform": "Android", "platformVersion": "10", "architecture": "", "model": "K", "mobile": w < 600,
                                                 "brands": [{"brand": "Chromium", "version": ver}]}})
                } else {
                    json!({"userAgent": ""}) // back to the browser's own
                };
                self.call("Emulation.setUserAgentOverride", ua).await?;
                Ok(format!("viewport {w}x{h}{}", if mobile { " mobile (touch, mobile user agent)" } else { "" }))
            }
            "theme" => {
                // prefers-color-scheme (and reduced motion) as the page's CSS and matchMedia see it
                let mut features = vec![];
                let mut said = vec![];
                for w in &a.pos {
                    match w.as_str() {
                        "dark" | "light" => { features.push(json!({"name": "prefers-color-scheme", "value": w})); said.push(w.clone()); }
                        "reduced-motion" => { features.push(json!({"name": "prefers-reduced-motion", "value": "reduce"})); said.push(w.clone()); }
                        "off" | "reset" => {}
                        o => bail!("theme takes dark|light [reduced-motion] or off, got {o}"),
                    }
                }
                if a.pos.is_empty() {
                    bail!("usage: theme dark|light [reduced-motion] | theme off");
                }
                self.call("Emulation.setEmulatedMedia", json!({ "features": features })).await?;
                Ok(if said.is_empty() { "theme: browser default".into() } else { format!("theme {}", said.join(" ")) })
            }
            "block" => {
                let mut types = vec![];
                for t in &a.pos {
                    match t.as_str() {
                        "off" | "none" => {
                            types.clear();
                            break;
                        }
                        "images" | "image" | "img" => types.push("Image"),
                        "fonts" | "font" => types.push("Font"),
                        "media" | "video" => types.push("Media"),
                        "css" | "styles" | "stylesheet" => types.push("Stylesheet"),
                        "lite" => types.extend(["Image", "Font", "Media"]),
                        other => bail!("unknown resource type '{other}' (images, fonts, media, css, lite, off)"),
                    }
                }
                let types: Vec<String> = types.into_iter().map(String::from).collect();
                let msg = if types.is_empty() { "blocking off".to_string() } else { format!("blocking {}", types.join(", ")) };
                self.set_blocking(types).await?;
                Ok(msg)
            }
            "mock" | "mocks" => {
                let mut mocks = self.st.lock().unwrap().mocks.clone();
                let Some(pattern) = a.pos.first().cloned() else {
                    if mocks.is_empty() {
                        return Ok("no mocks".into());
                    }
                    let lines: Vec<String> = mocks
                        .iter()
                        .map(|m| {
                            let what = if m.abort { "abort".into() } else if m.status == 0 { "pass".into() } else { m.status.to_string() };
                            let delay = if m.delay_ms > 0 { format!(" +{}ms", m.delay_ms) } else { String::new() };
                            format!("{} → {what}{delay} · {} hit(s)", m.pattern, m.hits)
                        })
                        .collect();
                    return Ok(lines.join("\n"));
                };
                if pattern == "off" || pattern == "clear" {
                    match a.pos.get(1) {
                        Some(p) => mocks.retain(|m| m.pattern != *p),
                        None => mocks.clear(),
                    }
                    let n = mocks.len();
                    self.set_mocks(mocks).await?;
                    return Ok(if n == 0 { "mocks off".into() } else { format!("mock off · {n} left") });
                }
                let second = a.pos.get(1).map(|s| s.as_str());
                let abort = matches!(second, Some("abort" | "fail" | "offline"));
                let mut status: u16 = match second {
                    Some(s) if !abort => s.parse().map_err(|_| anyhow!("expected a status code or `abort`, got '{s}'"))?,
                    _ => 0,
                };
                let (body, ctype): (Vec<u8>, String) = if let Some(j) = a.get(&["json"]) {
                    serde_json::from_str::<Value>(j).map_err(|e| anyhow!("--json is not valid JSON ({e}); quote it: --json '{{\"items\":[]}}'"))?;
                    (j.as_bytes().to_vec(), "application/json".into())
                } else if let Some(b) = a.get(&["body"]) {
                    (b.as_bytes().to_vec(), "text/plain".into())
                } else if let Some(f) = a.get(&["file"]) {
                    let path = ctx.cwd.join(f);
                    let bytes = std::fs::read(&path).map_err(|e| anyhow!("can't read {}: {e}", path.display()))?;
                    let ct = match path.extension().and_then(|x| x.to_str()).unwrap_or("") {
                        "json" => "application/json",
                        "html" | "htm" => "text/html",
                        "js" => "text/javascript",
                        "css" => "text/css",
                        "svg" => "image/svg+xml",
                        "png" => "image/png",
                        _ => "application/octet-stream",
                    };
                    (bytes, ct.into())
                } else {
                    (vec![], "text/plain".into())
                };
                let delay_ms: u64 = a.get(&["delay"]).map(|d| d.parse()).transpose().map_err(|_| anyhow!("--delay takes milliseconds"))?.unwrap_or(0);
                if status == 0 && !abort && (!body.is_empty() || delay_ms == 0) {
                    status = 200;
                }
                let ctype = a.get(&["type"]).map(String::from).unwrap_or(ctype);
                let m = Mock { pattern: pattern.clone(), status, body, ctype, abort, delay_ms, doc: a.has(&["doc"]), hits: 0 };
                let what = if abort { "abort".into() } else if status == 0 { "real response".into() } else { format!("{status}{}", if m.body.is_empty() { String::new() } else { format!(" ({} bytes)", m.body.len()) }) };
                let delay = if delay_ms > 0 { format!(" after {delay_ms}ms") } else { String::new() };
                mocks.retain(|x| x.pattern != pattern);
                mocks.push(m);
                self.set_mocks(mocks).await?;
                Ok(format!("mock {pattern} → {what}{delay} (applies from the next request; `wowsir reload` to re-fetch)"))
            }
            "dialog" | "dialogs" => {
                let accept = match a.pos.first().map(|s| s.as_str()) {
                    Some("accept") | None => true,
                    Some("dismiss") => false,
                    Some(o) => bail!("expected accept [TEXT] | dismiss, got {o}"),
                };
                let text = (a.pos.len() > 1).then(|| a.pos[1..].join(" "));
                if text.is_some() && !accept {
                    bail!("dismiss takes no text (prompt text only applies with `dialog accept TEXT`)");
                }
                let note = text.as_ref().map(|t| format!("; prompts get \"{t}\"")).unwrap_or_default();
                let mut st = self.st.lock().unwrap();
                st.dialog_accept = accept;
                st.dialog_text = text;
                Ok(format!("dialogs will be {}{note}", if accept { "accepted" } else { "dismissed" }))
            }
            "cookies" => {
                if a.pos.first().map(|s| s.as_str()) == Some("clear") {
                    self.call("Storage.clearCookies", json!({ "browserContextId": self.browser_ctx })).await?;
                    return Ok("cookies cleared".into());
                }
                let r = self.call("Network.getCookies", json!({})).await?;
                let lines: Vec<String> = r["cookies"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|c| {
                                format!(
                                    "{}={} ({}{})",
                                    c["name"].as_str().unwrap_or(""),
                                    truncate_chars(c["value"].as_str().unwrap_or(""), 40),
                                    c["domain"].as_str().unwrap_or(""),
                                    if c["httpOnly"].as_bool() == Some(true) { " httpOnly" } else { "" }
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(if lines.is_empty() { "(no cookies)".into() } else { lines.join("\n") })
            }
            "state" => {
                let sub = a.p(0, "save|load")?;
                let f = a.p(1, "file")?;
                let p = if Path::new(f).is_absolute() { PathBuf::from(f) } else { ctx.cwd.join(f) };
                match sub {
                    "save" => {
                        let r = self.call("Storage.getCookies", json!({ "browserContextId": self.browser_ctx })).await?;
                        let cookies: Vec<Value> = r["cookies"].as_array().map(|a| a.iter().map(crate::login::clean_cookie).collect()).unwrap_or_default();
                        let ls = self.js("JSON.stringify({origin: location.origin, localStorage: Object.entries(localStorage).map(([name,value])=>({name,value}))})").await.ok();
                        let origins: Vec<Value> = ls
                            .and_then(|v| v.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()))
                            .filter(|o| o["origin"].as_str().map_or(false, |s| s.starts_with("http")) && o["localStorage"].as_array().map_or(false, |a| !a.is_empty()))
                            .into_iter()
                            .collect();
                        let state = json!({ "cookies": cookies, "origins": origins });
                        crate::login::write_private(&p, &serde_json::to_string_pretty(&state)?)?;
                        Ok(format!("saved {} cookies, {} origin(s) to {}", cookies.len(), origins.len(), p.display()))
                    }
                    "load" => {
                        let (c, o) = self.load_state(&p).await?;
                        Ok(format!("loaded {c} cookies, {o} origin(s) from {}", p.display()))
                    }
                    o => bail!("state save|load FILE (got {o})"),
                }
            }
            "url" => Ok(self.url()),
            "title" => Ok(self.js("document.title").await?.as_str().unwrap_or("").to_string()),
            "record" => {
                match a.pos.first().map(|s| s.as_str()).unwrap_or("show") {
                    "show" => Ok(if self.history.is_empty() { "(nothing recorded yet)".into() } else { self.history.join("\n") }),
                    "clear" => {
                        self.history.clear();
                        self.record_url = self.url();
                        Ok("recording cleared".into())
                    }
                    "save" => {
                        let f = a.p(1, "file path")?;
                        let p = if Path::new(f).is_absolute() { PathBuf::from(f) } else { ctx.cwd.join(f) };
                        let mut steps = self.history.clone();
                        // a test must load its own page: add the one the recording started on
                        let opens = steps.first().map_or(false, |s| s.starts_with("open ") || s.starts_with("goto "));
                        let start = if self.record_url.is_empty() { self.url() } else { self.record_url.clone() };
                        if !opens && start.starts_with("http") {
                            steps.insert(0, shell_words::join(["open", start.as_str()]));
                        }
                        let body = format!(
                            "# wowsir test — recorded {}\n# run with: wowsir run {}\n{}\n",
                            self.name,
                            f,
                            steps.join("\n")
                        );
                        std::fs::write(&p, body)?;
                        Ok(format!("saved {} steps to {}", steps.len(), p.display()))
                    }
                    o => bail!("record show|save <file>|clear (got {o})"),
                }
            }
            _ => bail!("unknown command '{cmd}'. Run `wowsir help`."),
        }
    }

    async fn cmd_wait(&mut self, a: &Args, to: Duration) -> Result<String> {
        let first = a.p(0, "what to wait for (ms, text, gone, url, idle, or a target)")?;
        if let Ok(n) = first.parse::<u64>() {
            tokio::time::sleep(Duration::from_millis(n)).await;
            return Ok(format!("waited {n}ms"));
        }
        let start = Instant::now();
        let rest = a.pos[1..].join(" ");
        if first == "idle" {
            return Ok(match self.settle(to).await {
                None => format!("idle after {}ms", ms(start)),
                Some(n) => bail!("{n}"),
            });
        }
        loop {
            let (done, label) = match first {
                "text" => (self.k("hasText", &[json!(rest)]).await?.as_bool() == Some(true), format!("text \"{rest}\"")),
                "gone" | "hidden" | "no-text" => {
                    let gone = if first == "no-text" {
                        self.k("hasText", &[json!(rest)]).await?.as_bool() != Some(true)
                    } else {
                        let p = self.k("probe", &[json!(rest)]).await?;
                        !(p["found"].as_bool() == Some(true) && p["visible"].as_bool() == Some(true))
                    };
                    (gone, format!("{first} {rest}"))
                }
                "url" => (self.url().contains(&rest), format!("url ~ {rest}")),
                target => {
                    let p = self.k("probe", &[json!(target)]).await?;
                    (p["found"].as_bool() == Some(true) && p["visible"].as_bool() == Some(true), target.to_string())
                }
            };
            if done {
                return Ok(format!("wait {label} · {}ms", ms(start)));
            }
            if start.elapsed() >= to {
                bail!("timed out after {}ms waiting for {label}", to.as_millis());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    /// Assertions retry until the timeout (like Playwright's expect), so
    /// they're robust to async UI updates.
    async fn cmd_expect(&mut self, a: &Args, to: Duration) -> Result<String> {
        let kind = a.p(0, "assertion (text, no-text, url, title, visible, hidden, value, checked, count, no-errors, still)")?.to_string();
        if kind == "still" {
            // layout stability of watched elements since `watch` / the last `expect still`
            if self.watches.is_empty() {
                bail!("nothing watched: run `wowsir watch T...` before the steps, then `expect still`");
            }
            self.check_watches().await;
            let only: Vec<&String> = a.pos[1..].iter().collect();
            let bad: Vec<String> = self.moves.iter().filter(|(t, _)| only.is_empty() || only.contains(&t)).map(|(_, m)| m.clone()).collect();
            self.moves.retain(|(t, _)| !(only.is_empty() || only.contains(&t)));
            if bad.is_empty() {
                let n = if only.is_empty() { self.watches.len() } else { only.len() };
                return Ok(format!("✓ {n} watched element(s) stayed put"));
            }
            bail!("expected watched elements to stay put:\n  {}", bad.join("\n  "));
        }
        let start = Instant::now();
        loop {
            let (pass, label, actual) = self.check_once(&kind, a).await?;
            if pass {
                return Ok(format!("✓ {label}"));
            }
            if start.elapsed() >= to {
                let mut msg = format!("expected {label}");
                if let Some(act) = actual {
                    msg.push_str(&format!(", got {act}"));
                }
                bail!("{msg} (after {}ms)", to.as_millis());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn check_once(&mut self, kind: &str, a: &Args) -> Result<(bool, String, Option<String>)> {
        let rest = a.pos[1..].join(" ");
        let arg1 = a.pos.get(1).cloned().unwrap_or_default();
        let tail = a.pos.get(2..).map(|r| r.join(" ")).unwrap_or_default();
        Ok(match kind {
            "text" => (self.k("hasText", &[json!(rest)]).await?.as_bool() == Some(true), format!("text \"{rest}\" on page"), None),
            "no-text" | "not-text" => {
                (self.k("hasText", &[json!(rest)]).await?.as_bool() != Some(true), format!("no text \"{rest}\""), None)
            }
            "url" => {
                let u = self.url();
                (u.contains(&rest), format!("url contains \"{rest}\""), Some(u))
            }
            "title" => {
                let t = self.js("document.title").await?.as_str().unwrap_or("").to_string();
                (t.to_lowercase().contains(&rest.to_lowercase()), format!("title contains \"{rest}\""), Some(format!("\"{t}\"")))
            }
            "visible" | "hidden" => {
                let p = self.k("probe", &[json!(rest)]).await?;
                let vis = p["found"].as_bool() == Some(true) && p["visible"].as_bool() == Some(true);
                let state = if p["found"].as_bool() != Some(true) { "not found" } else if vis { "visible" } else { "hidden" };
                (vis == (kind == "visible"), format!("{rest} {kind}"), Some(state.into()))
            }
            "value" => {
                let v = self.k("value", &[json!(arg1)]).await;
                match v {
                    Ok(v) => {
                        let s = v.as_str().unwrap_or("").to_string();
                        (s == tail, format!("{arg1} value \"{tail}\""), Some(format!("\"{s}\"")))
                    }
                    Err(e) if e.to_string().contains("no element matches") => (false, format!("{arg1} value \"{tail}\""), Some("element not found".into())),
                    Err(e) => return Err(e),
                }
            }
            "checked" | "unchecked" | "mixed" => {
                let v = tri(&self.k("checked", &[json!(rest)]).await?);
                (v == kind, format!("{rest} {kind}"), Some(v.to_string()))
            }
            "count" => {
                let n: i64 = a.pos.last().and_then(|n| n.parse().ok()).ok_or_else(|| anyhow!("usage: expect count <target> <n>"))?;
                let t = a.pos[1..a.pos.len() - 1].join(" ");
                let c = self.k("count", &[json!(t)]).await?.as_i64().unwrap_or(0);
                (c == n, format!("{n} × {t}"), Some(c.to_string()))
            }
            "no-errors" | "noerrors" | "clean" => {
                let errs = self.st.lock().unwrap().errors.clone();
                let n = errs.len();
                (n == 0, "no console/network errors".into(), Some(format!("{n}:\n  {}", errs.join("\n  "))))
            }
            o => bail!("unknown assertion '{o}'. Try: text, no-text, url, title, visible, hidden, value, checked, unchecked, count, no-errors"),
        })
    }
}

use crate::login::looks_like_login;

/// checked state as reported by K.checked: true | false | "mixed"
fn tri(v: &Value) -> &'static str {
    match v.as_bool() {
        Some(true) => "checked",
        Some(false) => "unchecked",
        None => "mixed",
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!("{}\n… ({} more chars, use --max)", s.chars().take(max).collect::<String>(), n - max)
    }
}
