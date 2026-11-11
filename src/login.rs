//! `wowsir login URL`: open a visible browser, let a human log in, save the
//! resulting cookies as a Playwright-compatible storageState file.

use crate::browser;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Keep only the fields CDP's Storage.setCookies and Playwright both accept.
pub fn clean_cookie(c: &Value) -> Value {
    let mut o = json!({
        "name": c["name"], "value": c["value"], "domain": c["domain"], "path": c["path"],
        "httpOnly": c["httpOnly"].as_bool().unwrap_or(false),
        "secure": c["secure"].as_bool().unwrap_or(false),
    });
    let expires = c["expires"].as_f64().unwrap_or(-1.0);
    o["expires"] = json!(if c["session"].as_bool() == Some(true) { -1.0 } else { expires });
    if let Some(s) = c["sameSite"].as_str() {
        o["sameSite"] = json!(s);
    }
    o
}

/// Heuristic: does this URL look like a sign-in page?
pub fn looks_like_login(url: &str) -> bool {
    let u = url.to_lowercase();
    ["/login", "/signin", "/sign-in", "/oauth", "/authorize", "/auth/", "://auth.", "://login.", "://accounts."].iter().any(|k| u.contains(k))
}

pub fn host_of(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url).split(['/', ':', '?', '#']).next().unwrap_or("").to_string()
}

/// Where saved logins live. Every new session loads all of them.
pub fn auth_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WOWSIR_AUTH_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".wowsir/auth")
}

pub fn default_state_path(url: &str) -> PathBuf {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or("site").to_string();
    auth_dir().join(format!("{host}.json"))
}

/// Copy cookies whose domain contains `matching` out of a running debug
/// browser (e.g. Chrome started with --remote-debugging-port=9222).
pub fn import_from(browser_url: &str, matching: &str, out: &Path) -> Result<(usize, Vec<String>)> {
    use std::io::{Read, Write};
    let hostport = browser_url.trim_start_matches("http://").trim_end_matches('/');
    let mut s = std::net::TcpStream::connect(hostport).map_err(|e| anyhow!("cannot reach {browser_url}: {e}"))?;
    // Chrome's DevTools HTTP server keeps the socket open: read by Content-Length, not to EOF.
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(s, "GET /json/version HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n")?;
    let mut buf = vec![];
    let mut chunk = [0u8; 4096];
    let body = loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            bail!("{browser_url} closed the connection without a reply");
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(i) = text.find("\r\n\r\n") {
            let len = text[..i]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                break text[i + 4..i + 4 + len].to_string();
            }
        }
    };
    let body = body.as_str();
    let v: Value = serde_json::from_str(body)?;
    let ws_url = v["webSocketDebuggerUrl"].as_str().ok_or_else(|| anyhow!("no webSocketDebuggerUrl at {browser_url}"))?;
    let (mut ws, _) = tungstenite::connect(ws_url)?;
    ws.send(tungstenite::Message::Text(json!({"id": 1, "method": "Storage.getCookies", "params": {}}).to_string()))?;
    let cookies = loop {
        if let tungstenite::Message::Text(t) = ws.read()? {
            let m: Value = serde_json::from_str(&t)?;
            if m["id"] == 1 {
                break m["result"]["cookies"].as_array().cloned().unwrap_or_default();
            }
        }
    };
    let _ = ws.close(None);
    let picked: Vec<Value> = cookies
        .iter()
        .filter(|c| c["domain"].as_str().map_or(false, |d| d.contains(matching)))
        .map(clean_cookie)
        .collect();
    let mut domains: Vec<String> = picked.iter().filter_map(|c| c["domain"].as_str().map(String::from)).collect();
    domains.sort();
    domains.dedup();
    write_private(out, &serde_json::to_string_pretty(&json!({ "cookies": picked, "origins": [] }))?)?;
    Ok((picked.len(), domains))
}

pub fn write_private(path: &Path, body: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, body)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub async fn run(url: &str, out: &Path, timeout: Duration) -> Result<usize> {
    let b = browser::launch(true)?;
    let cdp = b.cdp.clone();
    cdp.call(None, "Target.createTarget", json!({ "url": url })).await?;
    let home_host = host_of(url);
    eprintln!(
        "A browser window is open at {url}.\nSign in there. The window closes by itself once you are back on {home_host}\n(or close it yourself when done). Cookies are saved to {}.",
        out.display()
    );

    let start = Instant::now();
    let mut last: Vec<Value> = vec![];
    let mut seen_page = false;
    let mut seen_login = false;
    let mut origins: Vec<Value> = vec![];
    let save = |cookies: &Vec<Value>, origins: &Vec<Value>| -> Result<()> {
        let state = json!({
            "cookies": cookies,
            "origins": origins,
            "wowsir": { "loginUrl": url, "savedAt": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }
        });
        write_private(out, &serde_json::to_string_pretty(&state)?)
    };
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // snapshot cookies continuously: on some platforms the browser exits with its last window
        if let Ok(r) = cdp.call(None, "Storage.getCookies", json!({})).await {
            if let Some(a) = r["cookies"].as_array() {
                last = a.iter().map(clean_cookie).collect();
            }
        }
        let urls: Vec<String> = match cdp.call(None, "Target.getTargets", json!({})).await {
            Ok(r) => r["targetInfos"]
                .as_array()
                .map(|a| a.iter().filter(|t| t["type"] == "page").filter_map(|t| t["url"].as_str().map(String::from)).collect())
                .unwrap_or_default(),
            Err(_) => vec![], // browser gone
        };
        if !urls.is_empty() {
            seen_page = true;
        }
        if urls.iter().any(|u| looks_like_login(u)) {
            seen_login = true;
        }
        // Done: went through a sign-in page and landed back on the site (any method: BankID, e-mail, SSO...)
        // Done: went through a sign-in page and landed back on the site (any method: BankID, Entra, e-mail...)
        // and the auth response (?code= / #code=) has been consumed by the app.
        if let Some(home) = urls.iter().find(|u| host_of(u) == home_host && !looks_like_login(u) && !u.contains("code=") && !u.contains("id_token=")) {
            if seen_login {
                let _ = home;
                tokio::time::sleep(Duration::from_millis(1500)).await; // let tokens/cookies land (MSAL writes storage after redirect)
                if let Ok(r) = cdp.call(None, "Storage.getCookies", json!({})).await {
                    if let Some(a) = r["cookies"].as_array() {
                        last = a.iter().map(clean_cookie).collect();
                    }
                }
                origins = capture_storage(&cdp, &home_host).await.into_iter().collect();
                break;
            }
        }
        if seen_page && urls.is_empty() {
            break; // user closed the window
        }
        if start.elapsed() > timeout {
            return Err(anyhow!("timed out after {}s waiting for sign-in", timeout.as_secs()));
        }
    }
    if last.is_empty() && origins.is_empty() {
        bail!("no cookies or storage were set: sign-in did not complete");
    }
    save(&last, &origins)?;
    drop(b); // closes the window
    Ok(last.len())
}

/// localStorage + sessionStorage of the first page on `host` (SPA logins such as MSAL/Entra keep tokens there).
async fn capture_storage(cdp: &std::sync::Arc<crate::cdp::Cdp>, host: &str) -> Option<Value> {
    let targets = cdp.call(None, "Target.getTargets", json!({})).await.ok()?;
    let t = targets["targetInfos"].as_array()?.iter().find(|t| t["type"] == "page" && t["url"].as_str().map_or(false, |u| host_of(u) == host))?.clone();
    let sid = cdp.call(None, "Target.attachToTarget", json!({ "targetId": t["targetId"], "flatten": true })).await.ok()?["sessionId"].as_str()?.to_string();
    let expr = "JSON.stringify({origin: location.origin, localStorage: Object.entries(localStorage).map(([name,value])=>({name,value})), sessionStorage: Object.entries(sessionStorage).map(([name,value])=>({name,value}))})";
    let r = cdp.call(Some(&sid), "Runtime.evaluate", json!({ "expression": expr, "returnByValue": true })).await.ok()?;
    let v: Value = serde_json::from_str(r["result"]["value"].as_str()?).ok()?;
    let empty = v["localStorage"].as_array().map_or(true, |a| a.is_empty()) && v["sessionStorage"].as_array().map_or(true, |a| a.is_empty());
    if empty { None } else { Some(v) }
}

/// Delete the saved login matching `what` (file name, or a host/URL whose cookies it holds).
/// Returns (file, cookie domains, storage origins) so running sessions can be wiped too.
pub fn forget(what: &str) -> Result<(PathBuf, Vec<String>, Vec<String>)> {
    let host = host_of(what);
    let host = if host.is_empty() { what.to_string() } else { host };
    let mut matches = vec![];
    for e in std::fs::read_dir(auth_dir())?.flatten() {
        let p = e.path();
        if p.extension().map_or(true, |x| x != "json") {
            continue;
        }
        let name = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
        let domains: Vec<String> = v["cookies"].as_array().map(|a| a.iter().filter_map(|c| c["domain"].as_str().map(String::from)).collect()).unwrap_or_default();
        let hit = name == what || name.contains(what) || domains.iter().any(|d| {
            let d = d.trim_start_matches('.');
            host == d || host.ends_with(&format!(".{d}"))
        });
        if hit {
            let origins = v["origins"].as_array().map(|a| a.iter().filter_map(|o| o["origin"].as_str().map(String::from)).collect()).unwrap_or_default();
            let mut ds = domains.clone();
            ds.sort();
            ds.dedup();
            matches.push((p, ds, origins));
        }
    }
    match matches.len() {
        0 => bail!("no saved login matches '{what}' (saved: {})", saved_logins().iter().map(|l| l.0.clone()).collect::<Vec<_>>().join(", ")),
        1 => {
            let m = matches.remove(0);
            std::fs::remove_file(&m.0)?;
            Ok(m)
        }
        _ => bail!("'{what}' matches several saved logins: {}", matches.iter().map(|m| m.0.file_stem().unwrap_or_default().to_string_lossy().to_string()).collect::<Vec<_>>().join(", ")),
    }
}

/// Saved logins: (name, earliest cookie expiry (unix secs, None = session-only), login URL)
pub fn saved_logins() -> Vec<(String, Option<f64>, Option<String>)> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(auth_dir()) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map_or(true, |x| x != "json") {
            continue;
        }
        let Ok(v) = std::fs::read_to_string(&p).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str::<Value>(&t)?)) else { continue };
        let exp = v["cookies"].as_array().and_then(|a| {
            a.iter().filter_map(|c| c["expires"].as_f64()).filter(|e| *e > 0.0).fold(None, |m: Option<f64>, e| Some(m.map_or(e, |m| m.min(e))))
        });
        let name = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
        out.push((name, exp, v["wowsir"]["loginUrl"].as_str().map(String::from)));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
