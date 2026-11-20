//! wowsir → Playwright: targets as Playwright locators, and .wowsir tests as
//! Playwright test files (`wowsir export`, `record save x.spec.ts`).
//!
//! The translation is static (no browser): explore cheaply with wowsir, then
//! hand a team a normal Playwright test for their CI and other browsers.

use crate::commands::{normalize_url, parse_args, split_line};

/// A JS string literal in single quotes.
pub fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\'' => o.push_str("\\'"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}

/// A JS regex literal matching `s` literally (case-insensitive with `i`).
fn re(s: &str, i: bool) -> String {
    let mut o = String::from("/");
    for c in s.chars() {
        if "\\^$.|?*+()[]{}/".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('/');
    if i {
        o.push('i');
    }
    o
}

/// A wowsir URL pattern (substring, or a glob over the whole URL) as a JS regex.
fn url_re(p: &str) -> String {
    if !p.contains('*') {
        return re(p, false);
    }
    let mut o = String::from("/^");
    for c in p.chars() {
        match c {
            '*' => o.push_str(".*"),
            '?' => o.push('.'),
            c if "\\^$.|+()[]{}/".contains(c) => {
                o.push('\\');
                o.push(c);
            }
            c => o.push(c),
        }
    }
    o.push_str("$/");
    o
}

const ROLES: &[&str] = &[
    "link",
    "button",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "treeitem",
    "clickable",
    "heading",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "img",
    "dialog",
    "row",
    "cell",
    "navigation",
    "main",
    "form",
    "region",
    "list",
    "listitem",
    "table",
    "alert",
    "status",
    "tabpanel",
    "input",
    "field",
    "banner",
    "contentinfo",
    "group",
    "progressbar",
    "h",
];

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// One target without ` >> `.
fn atom(t: &str) -> String {
    let t = t.trim();
    let bare = t.strip_prefix('@').unwrap_or(t);
    if bare.len() > 1 && bare.starts_with('e') && bare[1..].chars().all(|c| c.is_ascii_digit()) {
        return format!(
            "page.locator({}) /* TODO: {t} is a wowsir snapshot ref; use a stable target */",
            js(t)
        );
    }
    if let Some(v) = t.strip_prefix("text=") {
        return format!("page.getByText({})", js(unquote(v)));
    }
    if let Some((r, v)) = t.split_once([':', '=']) {
        if ROLES.contains(&r) {
            let name = js(unquote(v));
            return match r {
                "clickable" => format!("page.getByText({name}, {{ exact: true }})"),
                "input" | "field" => {
                    format!("page.getByRole('textbox', {{ name: {name}, exact: true }})")
                }
                "h" | "heading" => {
                    format!("page.getByRole('heading', {{ name: {name}, exact: true }})")
                }
                h if h.len() == 2 && h.starts_with('h') => {
                    format!(
                        "page.getByRole('heading', {{ name: {name}, level: {}, exact: true }})",
                        &h[1..]
                    )
                }
                r => format!("page.getByRole('{r}', {{ name: {name}, exact: true }})"),
            };
        }
    }
    // [data-testid="x"] → getByTestId (Playwright's default test id attribute)
    if let Some(inner) = t
        .strip_prefix("[data-testid=")
        .and_then(|s| s.strip_suffix(']'))
    {
        return format!("page.getByTestId({})", js(unquote(inner)));
    }
    format!("page.locator({})", js(t))
}

/// A wowsir target as a Playwright locator expression. `A >> B` becomes
/// `near(A, B)` when `helper` (the exported test defines it), else inline.
pub fn locator(t: &str, helper: bool) -> String {
    let parts: Vec<&str> = split_chain(t);
    let mut l = atom(parts[0]);
    for p in &parts[1..] {
        let b = atom(p);
        l = if helper {
            format!("near({l}, {b})")
        } else {
            format!("page.locator('*', {{ has: {l} }}).filter({{ has: {b} }}).last().locator({b})")
        };
    }
    l
}

/// Split on ` >> ` (with whitespace around it), like inject.js.
fn split_chain(t: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut rest = t;
    while let Some(i) = rest.find(">>") {
        let (a, b) = (&rest[..i], &rest[i + 2..]);
        if a.ends_with(char::is_whitespace) && b.starts_with(char::is_whitespace) {
            out.push(a.trim());
            rest = b.trim_start();
        } else {
            break;
        }
    }
    out.push(rest.trim());
    out
}

const NEAR: &str = "// wowsir's `A >> B`: the B inside the closest element around A that has one\n\
const near = (a: Locator, b: Locator) =>\n  a.page().locator('*', { has: a }).filter({ has: b }).last().locator(b);\n";

#[derive(Default)]
struct Spec {
    body: Vec<String>,
    near: bool,
    errors: bool,
    blocked: bool,
    mocked: std::collections::HashSet<String>,
}

impl Spec {
    fn line(&mut self, s: impl Into<String>) {
        self.body.push(s.into());
    }
    fn loc(&mut self, t: &str) -> String {
        if split_chain(t).len() > 1 {
            self.near = true;
        }
        locator(t, true)
    }
    fn todo(&mut self, raw: &str, why: &str) {
        self.line(format!("// wowsir: {raw}  ({why})"));
    }

    fn step(&mut self, w: &[String], raw: &str) {
        let Some(cmd) = w.first().map(|s| s.as_str()) else {
            return;
        };
        let a = parse_args(&w[1..]);
        let pos: Vec<&str> = a.pos.iter().map(|s| s.as_str()).collect();
        let p0 = pos.first().copied().unwrap_or("");
        let rest = |i: usize| pos.get(i..).map(|r| r.join(" ")).unwrap_or_default();
        // one-target commands read the rest of the line, as wowsir does: `click #tabs [data-tab=x]`
        let whole = rest(0);
        match cmd {
            "open" | "goto" | "navigate" => {
                self.line(format!("await page.goto({});", js(&normalize_url(p0))))
            }
            "back" => self.line("await page.goBack();"),
            "forward" => self.line("await page.goForward();"),
            "reload" => self.line("await page.reload();"),
            "click" | "dblclick" | "tap" => {
                let at = a
                    .get(&["at"])
                    .or(if cmd == "tap" { Some(p0) } else { None });
                if let Some((x, y)) = at.and_then(|p| p.split_once(',')) {
                    let (x, y) = (x.trim(), y.trim());
                    if x.parse::<f64>().is_ok() && y.parse::<f64>().is_ok() {
                        return self.line(match cmd {
                            "tap" => format!("await page.touchscreen.tap({x}, {y}); // needs hasTouch in the config"),
                            _ if cmd == "dblclick" || a.has(&["double"]) => format!("await page.mouse.dblclick({x}, {y});"),
                            _ => format!("await page.mouse.click({x}, {y});"),
                        });
                    }
                }
                let l = self.loc(&whole);
                if cmd == "tap" {
                    return self.line(format!("await {l}.tap(); // needs hasTouch in the config"));
                }
                let mut opts = vec![];
                if a.has(&["right"]) {
                    opts.push("button: 'right'".to_string());
                }
                let mods: Vec<&str> = [
                    ("alt", "Alt"),
                    ("ctrl", "Control"),
                    ("meta", "Meta"),
                    ("shift", "Shift"),
                    ("mod", "ControlOrMeta"),
                ]
                .iter()
                .filter(|(f, _)| a.has(&[f]))
                .map(|(_, m)| *m)
                .collect();
                if !mods.is_empty() {
                    opts.push(format!(
                        "modifiers: [{}]",
                        mods.iter().map(|m| js(m)).collect::<Vec<_>>().join(", ")
                    ));
                }
                let m = if cmd == "dblclick" || a.has(&["double"]) {
                    "dblclick"
                } else {
                    "click"
                };
                let o = if opts.is_empty() {
                    String::new()
                } else {
                    format!("{{ {} }}", opts.join(", "))
                };
                self.line(format!("await {l}.{m}({o});"));
            }
            "fill" => {
                let l = self.loc(p0);
                self.line(format!("await {l}.fill({});", js(&rest(1))));
            }
            "type" => {
                if pos.len() >= 2 {
                    let l = self.loc(p0);
                    self.line(format!("await {l}.pressSequentially({});", js(&rest(1))));
                } else {
                    self.line(format!("await page.keyboard.type({});", js(p0)));
                }
            }
            "press" | "key" => {
                for k in &pos {
                    self.line(format!("await page.keyboard.press({});", js(&key(k))));
                }
            }
            "select" => {
                let l = self.loc(p0);
                let v: Vec<String> = pos[1..].iter().map(|s| js(s)).collect();
                let arg = if v.len() == 1 {
                    v[0].clone()
                } else {
                    format!("[{}]", v.join(", "))
                };
                self.line(format!("await {l}.selectOption({arg});"));
            }
            "check" | "uncheck" | "hover" | "focus" => {
                let l = self.loc(&whole);
                self.line(format!("await {l}.{cmd}();"));
            }
            "upload" => {
                let l = self.loc(p0);
                let f: Vec<String> = pos[1..].iter().map(|s| js(s)).collect();
                self.line(format!("await {l}.setInputFiles([{}]);", f.join(", ")));
            }
            "scroll" => match p0 {
                "" | "down" | "up" => {
                    let px: i64 = pos.get(1).and_then(|p| p.parse().ok()).unwrap_or(600);
                    let px = if p0 == "up" { -px } else { px };
                    self.line(format!("await page.mouse.wheel(0, {px});"));
                }
                "top" => self.line("await page.evaluate(() => window.scrollTo(0, 0));"),
                "bottom" => self.line(
                    "await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));",
                ),
                t => {
                    let l = self.loc(t);
                    match pos.get(1).copied() {
                        None => self.line(format!("await {l}.scrollIntoViewIfNeeded();")),
                        Some(d) => {
                            let px: i64 = match d {
                                "down" => pos.get(2).and_then(|p| p.parse().ok()).unwrap_or(300),
                                "up" => -pos
                                    .get(2)
                                    .and_then(|p| p.parse::<i64>().ok())
                                    .unwrap_or(300),
                                n => n.parse().unwrap_or(300),
                            };
                            self.line(format!("await {l}.evaluate((e) => e.scrollBy(0, {px}));"));
                        }
                    }
                }
            },
            "wait" => {
                if let Ok(n) = p0.parse::<u64>() {
                    return self.line(format!("await page.waitForTimeout({n});"));
                }
                let r = rest(1);
                match p0 {
                    "idle" => self.line("await page.waitForLoadState('networkidle');"),
                    "text" => self.line(format!("await expect(page.getByText({}).filter({{ visible: true }}).first()).toBeVisible();", js(&r))),
                    "no-text" => self.line(format!("await expect(page.getByText({}).filter({{ visible: true }})).toHaveCount(0);", js(&r))),
                    "gone" | "hidden" => {
                        let l = self.loc(&r);
                        self.line(format!("await expect({l}).toBeHidden();"));
                    }
                    "url" => self.line(format!("await page.waitForURL((u) => u.href.includes({}));", js(&r))),
                    _ => {
                        let l = self.loc(&rest(0));
                        self.line(format!("await expect({l}).toBeVisible();"));
                    }
                }
            }
            "expect" | "assert" => self.expect(&pos, raw),
            "viewport" | "resize" => {
                let (w, h) = match p0 {
                    "mobile" | "iphone" => (390, 844),
                    "tablet" | "ipad" => (820, 1180),
                    "desktop" => (1280, 800),
                    "hd" => (1920, 1080),
                    s => {
                        let (w, h) = s
                            .split_once('x')
                            .unwrap_or((s, pos.get(1).copied().unwrap_or("")));
                        match (w.parse::<u32>(), h.parse::<u32>()) {
                            (Ok(w), Ok(h)) => (w, h),
                            _ => return self.todo(raw, "unrecognized viewport"),
                        }
                    }
                };
                self.line(format!(
                    "await page.setViewportSize({{ width: {w}, height: {h} }});"
                ));
                if matches!(p0, "mobile" | "iphone" | "tablet" | "ipad") || a.has(&["mobile", "m"])
                {
                    self.line("// wowsir also emulated touch and a mobile user agent: use a device from `devices` in the config");
                }
            }
            "theme" => {
                let scheme = match p0 {
                    "dark" | "light" => js(p0),
                    _ => "null".into(),
                };
                let motion = if pos.contains(&"reduced-motion") {
                    "'reduce'"
                } else if p0 == "off" {
                    "null"
                } else {
                    "'no-preference'"
                };
                self.line(format!("await page.emulateMedia({{ colorScheme: {scheme}, reducedMotion: {motion} }});"));
            }
            "dialog" => {
                let how = match p0 {
                    "dismiss" => "d.dismiss()".to_string(),
                    _ if pos.len() > 1 => format!("d.accept({})", js(&rest(1))),
                    _ => "d.accept()".to_string(),
                };
                self.line("page.removeAllListeners('dialog');");
                self.line(format!("page.on('dialog', (d) => {how});"));
            }
            "block" => {
                let mut types = vec![];
                for t in &pos {
                    match *t {
                        "images" | "image" | "img" => types.push("image"),
                        "fonts" | "font" => types.push("font"),
                        "media" | "video" => types.push("media"),
                        "css" | "styles" | "stylesheet" => types.push("stylesheet"),
                        "lite" => types.extend(["image", "font", "media"]),
                        _ => {}
                    }
                }
                if types.is_empty() {
                    return self.todo(raw, "unroute the block route if one was added");
                }
                let list = types.iter().map(|t| js(t)).collect::<Vec<_>>().join(", ");
                self.line(format!(
                    "await page.route(/.*/, (route) => ([{list}].includes(route.request().resourceType()) ? route.abort() : route.fallback()));"
                ));
            }
            "mock" | "mocks" => self.mock(&a, raw),
            "readonly" => match p0 {
                "on" | "strict" => {
                    let strict = p0 == "strict";
                    if strict {
                        self.blocked = true;
                    }
                    self.line("// wowsir read-only: writes are blocked (by method; wowsir also blocks logout-style GETs and WebSocket sends)");
                    self.line("await page.route(/.*/, (route) => {");
                    self.line("  if (!['POST', 'PUT', 'PATCH', 'DELETE'].includes(route.request().method())) return route.fallback();");
                    if strict {
                        self.line("  blocked.push(`${route.request().method()} ${route.request().url()}`);");
                    }
                    self.line("  return route.abort();");
                    self.line("});");
                }
                "off" => self.line("await page.unroute(/.*/);"),
                _ => self.todo(raw, "no Playwright equivalent"),
            },
            "screenshot" => {
                let path = pos
                    .first()
                    .filter(|p| !p.starts_with('-'))
                    .copied()
                    .unwrap_or("screenshot.png");
                let full = if a.has(&["full"]) {
                    ", fullPage: true"
                } else {
                    ""
                };
                match a.get(&["el"]) {
                    Some(t) => {
                        let l = self.loc(t);
                        self.line(format!("await {l}.screenshot({{ path: {} }});", js(path)));
                    }
                    None => self.line(format!(
                        "await page.screenshot({{ path: {}{full} }});",
                        js(path)
                    )),
                }
            }
            "eval" | "js" if !p0.is_empty() && !a.has(&["file"]) => {
                let code = rest(0)
                    .replace('\\', "\\\\")
                    .replace('`', "\\`")
                    .replace("${", "\\${");
                self.line(format!("await page.evaluate(`{code}`);"));
            }
            "tab" | "tabs" => self.todo(
                raw,
                "wowsir follows new tabs itself; in Playwright use context.waitForEvent('page')",
            ),
            "watch" => self.todo(raw, "layout watching has no Playwright equivalent"),
            "login" | "state" => self.todo(raw, "use storageState in the Playwright config"),
            _ => self.todo(raw, "not an action; skipped"),
        }
    }

    fn expect(&mut self, pos: &[&str], raw: &str) {
        let kind = pos.first().copied().unwrap_or("");
        let rest = pos.get(1..).map(|r| r.join(" ")).unwrap_or_default();
        match kind {
            // wowsir looks at visible text only (a closed dialog's text doesn't count)
            "text" => self.line(format!("await expect(page.getByText({}).filter({{ visible: true }}).first()).toBeVisible();", js(&rest))),
            "no-text" | "not-text" => self.line(format!("await expect(page.getByText({}).filter({{ visible: true }})).toHaveCount(0);", js(&rest))),
            "url" => self.line(format!("await expect(page).toHaveURL({});", re(&rest, false))),
            "title" => self.line(format!("await expect(page).toHaveTitle({});", re(&rest, true))),
            "visible" | "hidden" => {
                let l = self.loc(&rest);
                let m = if kind == "visible" { "toBeVisible" } else { "toBeHidden" };
                self.line(format!("await expect({l}).{m}();"));
            }
            "value" if pos.len() >= 2 => {
                let l = self.loc(pos[1]);
                let v = pos.get(2..).map(|r| r.join(" ")).unwrap_or_default();
                self.line(format!("await expect({l}).toHaveValue({});", js(&v)));
            }
            "checked" | "unchecked" | "mixed" => {
                let l = self.loc(&rest);
                self.line(match kind {
                    "checked" => format!("await expect({l}).toBeChecked();"),
                    "unchecked" => format!("await expect({l}).not.toBeChecked();"),
                    _ => format!("await expect({l}).toBeChecked({{ indeterminate: true }});"),
                });
            }
            "count" if pos.len() >= 3 => {
                let n = pos[pos.len() - 1];
                let l = self.loc(&pos[1..pos.len() - 1].join(" "));
                self.line(format!("await expect({l}).toHaveCount({n});"));
            }
            "no-errors" | "noerrors" | "clean" => {
                self.errors = true;
                self.line("expect(errors, 'console and page errors').toEqual([]);");
            }
            _ => self.todo(raw, "no Playwright equivalent"),
        }
    }

    fn mock(&mut self, a: &crate::commands::Args, raw: &str) {
        let is_methods = |m: &str| {
            !m.is_empty()
                && m.split('|').all(|x| {
                    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"].contains(&x)
                })
        };
        let mut pos: Vec<&str> = a.pos.iter().map(|s| s.as_str()).collect();
        let methods: Vec<&str> = match pos.first() {
            Some(m) if is_methods(m) => pos.remove(0).split('|').collect(),
            _ => vec![],
        };
        let Some(pattern) = pos.first().copied() else {
            return self.todo(raw, "lists mocks; skipped");
        };
        if pattern == "off" || pattern == "clear" {
            return self.line(match pos.get(1) {
                Some(p) => format!("await page.unroute({});", url_re(p)),
                None => "await page.unrouteAll();".into(),
            });
        }
        let what = pos.get(1).copied();
        let body = a.get(&["json"]).or(a.get(&["body"])).or(a.get(&["file"]));
        // like wowsir: a new mock for a pattern replaces the old one
        if !self.mocked.insert(pattern.to_string()) {
            self.line(format!("await page.unroute({});", url_re(pattern)));
        }
        self.line(format!(
            "await page.route({}, async (route) => {{",
            url_re(pattern)
        ));
        if !methods.is_empty() {
            let list = methods.iter().map(|m| js(m)).collect::<Vec<_>>().join(", ");
            self.line(format!(
                "  if (![{list}].includes(route.request().method())) return route.fallback();"
            ));
        }
        if !a.has(&["doc"]) {
            self.line(
                "  if (route.request().resourceType() === 'document') return route.fallback();",
            );
        }
        if let Some(d) = a.get(&["delay"]) {
            self.line(format!("  await new Promise((r) => setTimeout(r, {d}));"));
        }
        if matches!(what, Some("abort" | "fail" | "offline")) {
            self.line("  await route.abort();");
        } else if what.is_none() && body.is_none() && a.has(&["delay"]) {
            // only slowed down: the real response, late
            self.line("  await route.continue();");
        } else {
            let status = what.and_then(|w| w.parse::<u16>().ok()).unwrap_or(200);
            let mut f = vec![format!("status: {status}")];
            if let Some(j) = a.get(&["json"]) {
                f.push("contentType: 'application/json'".into());
                f.push(format!("body: {}", js(j)));
            } else if let Some(b) = a.get(&["body"]) {
                f.push(format!("body: {}", js(b)));
            } else if let Some(p) = a.get(&["file"]) {
                f.push(format!("path: {}", js(p)));
            }
            if let Some(t) = a.get(&["type"]) {
                f.retain(|x| !x.starts_with("contentType"));
                f.push(format!("contentType: {}", js(t)));
            }
            self.line(format!("  await route.fulfill({{ {} }});", f.join(", ")));
        }
        self.line("});");
    }
}

/// wowsir key names → Playwright's.
fn key(k: &str) -> String {
    k.split('+')
        .map(|p| match p.to_ascii_lowercase().as_str() {
            "mod" => "ControlOrMeta".to_string(),
            "ctrl" | "control" => "Control".to_string(),
            "cmd" | "command" | "meta" | "super" => "Meta".to_string(),
            "esc" | "escape" => "Escape".to_string(),
            "alt" | "option" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "return" | "enter" => "Enter".to_string(),
            "del" | "delete" => "Delete".to_string(),
            "space" => "Space".to_string(),
            _ => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// A .wowsir test (its text) as a Playwright test file.
pub fn spec(src: &str, name: &str, from: &str) -> Result<String, String> {
    let mut s = Spec::default();
    for (n, line) in src.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() {
            if s.body.last().is_some_and(|b| !b.is_empty()) {
                s.line("");
            }
            continue;
        }
        if let Some(c) = l.strip_prefix('#') {
            // the file's own header ("run with: wowsir run x") doesn't apply any more
            if !(c.contains("run with:") || c.trim_start().starts_with("wowsir test")) {
                s.line(format!("//{c}"));
            }
            continue;
        }
        let l = l.strip_prefix("wowsir ").unwrap_or(l);
        let w = if let Some(w) = crate::commands::eval_line(l) {
            w
        } else {
            split_line(l).map_err(|e| format!("{from} line {}: {e}", n + 1))?
        };
        s.step(&w, l);
    }
    while s.body.last().is_some_and(|b| b.is_empty()) {
        s.body.pop();
    }
    let mut out = format!(
        "// Playwright test converted by `wowsir export` from {from}\n\
         // Unlike wowsir, Playwright needs frameLocator() for elements in iframes, can't reach\n\
         // closed shadow DOM, and doesn't follow new tabs: adjust those steps by hand.\n\
         import {{ test, expect{} }} from '@playwright/test';\n\n",
        if s.near { ", type Locator" } else { "" }
    );
    if s.near {
        out.push_str(NEAR);
        out.push('\n');
    }
    out.push_str(&format!("test({}, async ({{ page }}) => {{\n", js(name)));
    if s.errors {
        out.push_str("  // what wowsir's `expect no-errors` checks: console errors and uncaught exceptions\n");
        out.push_str("  const errors: string[] = [];\n");
        out.push_str("  page.on('pageerror', (e) => errors.push(e.message));\n");
        out.push_str("  page.on('console', (m) => {\n");
        out.push_str("    if (m.type() === 'error' && !m.location().url.includes('favicon')) errors.push(m.text());\n");
        out.push_str("  });\n");
    }
    if s.blocked {
        out.push_str("  const blocked: string[] = []; // writes stopped by read-only (strict)\n");
    }
    if s.errors || s.blocked {
        out.push('\n');
    }
    for b in &s.body {
        if b.is_empty() {
            out.push('\n');
        } else {
            out.push_str("  ");
            out.push_str(b);
            out.push('\n');
        }
    }
    if s.blocked {
        out.push_str("  expect(blocked, 'tried to write while read-only').toEqual([]);\n");
    }
    out.push_str("});\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locators() {
        assert_eq!(
            locator("button:Sign in", false),
            "page.getByRole('button', { name: 'Sign in', exact: true })"
        );
        assert_eq!(
            locator("h2:Shipping", false),
            "page.getByRole('heading', { name: 'Shipping', level: 2, exact: true })"
        );
        assert_eq!(
            locator("text=It's here", false),
            "page.getByText('It\\'s here')"
        );
        assert_eq!(
            locator("[data-testid=\"submit\"]", false),
            "page.getByTestId('submit')"
        );
        assert_eq!(locator("#q", false), "page.locator('#q')");
        assert_eq!(
            locator("text=Walk dog >> button:Delete", true),
            "near(page.getByText('Walk dog'), page.getByRole('button', { name: 'Delete', exact: true }))"
        );
        assert!(locator("text=Walk dog >> button:Delete", false)
            .starts_with("page.locator('*', { has: page.getByText('Walk dog') })"));
        assert!(locator("@e12", false).contains("TODO"));
    }

    #[test]
    fn patterns() {
        assert_eq!(url_re("/api/items"), "/\\/api\\/items/");
        assert_eq!(url_re("*/api/*.json"), "/^.*\\/api\\/.*\\.json$/");
        assert_eq!(key("Mod+Enter"), "ControlOrMeta+Enter");
    }

    #[test]
    fn a_test_file() {
        let src = "# sign in\nopen localhost:8765\nfill textbox:Email a@b.c\nclick 'text=Walk dog >> button:Delete'\nexpect text Welcome\nexpect no-errors\nsnap -i\n";
        let out = spec(src, "login", "login.wowsir").unwrap();
        assert!(out.contains("import { test, expect, type Locator } from '@playwright/test';"));
        assert!(out.contains("const near ="));
        assert!(out.contains("  // sign in\n  await page.goto('http://localhost:8765');"));
        assert!(out.contains(
            "await page.getByRole('textbox', { name: 'Email', exact: true }).fill('a@b.c');"
        ));
        assert!(out.contains("await near(page.getByText('Walk dog'), page.getByRole('button', { name: 'Delete', exact: true })).click();"));
        assert!(out.contains("await expect(page.getByText('Welcome').filter({ visible: true }).first()).toBeVisible();"));
        assert!(out.contains("const errors: string[] = [];"));
        assert!(out.contains("// wowsir: snap -i  (not an action; skipped)"));
    }

    #[test]
    fn unquoted_selector_with_spaces() {
        let out = spec(
            "click #live-tabs [data-tab=nearby]\nhover .menu a.active\n",
            "t",
            "t.wowsir",
        )
        .unwrap();
        assert!(
            out.contains("await page.locator('#live-tabs [data-tab=nearby]').click();"),
            "{out}"
        );
        assert!(
            out.contains("await page.locator('.menu a.active').hover();"),
            "{out}"
        );
    }
}
