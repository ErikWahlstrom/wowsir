//! Background daemon: owns one Chromium process; each named session is an
//! isolated browser context inside it (new session ≈ 10ms, not a new browser).

use crate::browser::{self, Browser};
use crate::commands::Ctx;
use crate::session::{Session, TabEvent};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WOWSIR_HOME") {
        return PathBuf::from(d);
    }
    let uid = unsafe { libc::getuid() };
    // fixed path (not $TMPDIR) so every caller finds the same daemon
    PathBuf::from(format!("/tmp/wowsir-{uid}"))
}

pub fn sock_path() -> PathBuf {
    let p = dir().join("wowsir.sock");
    // unix socket paths max out around 104 bytes: fall back to a short, stable path
    if p.as_os_str().len() > 100 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        p.hash(&mut h);
        return PathBuf::from(format!(
            "/tmp/wowsir-{}-{:x}.sock",
            unsafe { libc::getuid() },
            h.finish()
        ));
    }
    p
}

#[derive(Deserialize, Serialize, Default, Clone)]
pub struct Req {
    pub session: String,
    pub cmd: Vec<String>,
    #[serde(default)]
    pub batch: Option<Vec<(usize, Vec<String>)>>,
    pub cwd: String,
    #[serde(default)]
    pub quiet: bool,
    #[serde(default)]
    pub full_after: bool,
    #[serde(default)]
    pub headed: bool,
    #[serde(default)]
    pub timeout: Option<u64>,
    /// don't load saved logins into a newly created session
    #[serde(default)]
    pub no_auth: bool,
    /// the client's build_id(): a daemon started from another build says so
    #[serde(default)]
    pub build: String,
}

/// Identifies the installed binary (mtime + size), so a daemon left running
/// across an upgrade can tell that it is out of date.
pub fn build_id() -> String {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{t}-{}", m.len())
        })
        .unwrap_or_default()
}

#[derive(Deserialize, Serialize)]
pub struct Resp {
    pub out: String,
    pub code: i32,
}

/// A session's queue of tab opened/closed events, filled by the browser-level event loop.
type TabQueue = Arc<std::sync::Mutex<Vec<TabEvent>>>;

struct Daemon {
    browser: Mutex<Option<Browser>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    headed: bool,
    /// browserContextId -> that session's tab event queue
    contexts: Arc<std::sync::Mutex<HashMap<String, TabQueue>>>,
    last_used: std::sync::Mutex<Instant>,
    build: String,
    /// sessions already told that the daemon is older than the client
    warned: std::sync::Mutex<std::collections::HashSet<String>>,
}

pub async fn run(headed: bool) -> Result<()> {
    let d = dir();
    std::fs::create_dir_all(&d)?;
    // single-instance lock
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(d.join("daemon.lock"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(()); // another daemon is starting/running
    }
    let sock = sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;

    let daemon = Arc::new(Daemon {
        browser: Mutex::new(None),
        sessions: Mutex::new(HashMap::new()),
        headed,
        contexts: Arc::new(std::sync::Mutex::new(HashMap::new())),
        last_used: std::sync::Mutex::new(Instant::now()),
        build: build_id(),
        warned: Default::default(),
    });
    // warm the browser immediately; the first command is already on its way
    {
        let d2 = daemon.clone();
        tokio::spawn(async move {
            let _ = d2.ensure_browser().await;
        });
    }
    // idle shutdown
    {
        let d2 = daemon.clone();
        let idle = Duration::from_secs(
            std::env::var("WOWSIR_IDLE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30 * 60),
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if d2.last_used.lock().unwrap().elapsed() > idle {
                    d2.shutdown().await;
                }
            }
        });
    }
    std::mem::forget(lock); // keep the flock for the life of the process
    loop {
        let (stream, _) = listener.accept().await?;
        let d2 = daemon.clone();
        tokio::spawn(async move {
            let _ = d2.handle(stream).await;
        });
    }
}

impl Daemon {
    async fn shutdown(&self) -> ! {
        for (_, s) in self.sessions.lock().await.drain() {
            s.lock().await.close().await;
        }
        drop(self.browser.lock().await.take());
        let _ = std::fs::remove_file(sock_path());
        std::process::exit(0);
    }

    async fn ensure_browser(&self) -> Result<Arc<crate::cdp::Cdp>> {
        let mut b = self.browser.lock().await;
        let dead = match b.as_mut() {
            Some(br) => br.child.try_wait().ok().flatten().is_some(),
            None => true,
        };
        if dead {
            self.sessions.lock().await.clear();
            *b = None;
            let br = browser::launch(self.headed)?;
            // follow pages the sites open (popups, target=_blank) and close
            let mut rx = br.cdp.subscribe("");
            let contexts = self.contexts.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let p = &ev.params;
                    match ev.method.as_str() {
                        "Target.targetCreated" if p["targetInfo"]["type"] == "page" => {
                            let ctx = p["targetInfo"]["browserContextId"].as_str().unwrap_or("");
                            if let Some(q) = contexts.lock().unwrap().get(ctx) {
                                q.lock().unwrap().push(TabEvent::Created {
                                    target: p["targetInfo"]["targetId"]
                                        .as_str()
                                        .unwrap_or("")
                                        .to_string(),
                                    url: p["targetInfo"]["url"].as_str().unwrap_or("").to_string(),
                                });
                            }
                        }
                        "Target.targetDestroyed" => {
                            let id = p["targetId"].as_str().unwrap_or("").to_string();
                            for q in contexts.lock().unwrap().values() {
                                q.lock().unwrap().push(TabEvent::Destroyed(id.clone()));
                            }
                        }
                        _ => {}
                    }
                }
            });
            let _ = br
                .cdp
                .call(
                    None,
                    "Target.setDiscoverTargets",
                    serde_json::json!({"discover": true}),
                )
                .await;
            *b = Some(br);
        }
        Ok(b.as_ref().unwrap().cdp.clone())
    }

    async fn session(&self, name: &str, no_auth: bool) -> Result<Arc<Mutex<Session>>> {
        if let Some(s) = self.sessions.lock().await.get(name) {
            return Ok(s.clone());
        }
        let mut last_err = None;
        for _ in 0..2 {
            let cdp = self.ensure_browser().await?;
            match Session::create(cdp, name).await {
                Ok(mut s) => {
                    if !no_auth {
                        s.load_auth_dir().await;
                    }
                    self.contexts
                        .lock()
                        .unwrap()
                        .insert(s.browser_ctx.clone(), s.tab_events.clone());
                    let s = Arc::new(Mutex::new(s));
                    self.sessions
                        .lock()
                        .await
                        .insert(name.to_string(), s.clone());
                    return Ok(s);
                }
                Err(e) => {
                    // browser probably died: force relaunch
                    if let Some(mut b) = self.browser.lock().await.take() {
                        let _ = b.child.kill();
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap())
    }

    async fn handle(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (r, mut w) = stream.into_split();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await?;
        let req: Req = serde_json::from_str(&line)?;
        *self.last_used.lock().unwrap() = Instant::now();
        let stale = !req.build.is_empty()
            && req.build != self.build
            && self.warned.lock().unwrap().insert(req.session.clone());
        let mut resp = self.process(req).await;
        if stale && resp.code != 99 {
            resp.out.push_str("\n! wowsir was updated after this daemon started, so commands still run the old version. `wowsir stop` switches to the new one (it ends running sessions, including other agents').");
        }
        if resp.code == 99 {
            let _ = std::fs::remove_file(sock_path());
        }
        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        w.write_all(&bytes).await?;
        w.flush().await?;
        if resp.code == 99 {
            self.shutdown().await;
        }
        Ok(())
    }

    async fn process(&self, req: Req) -> Resp {
        let cmd = req.cmd.first().map(|s| s.as_str()).unwrap_or("");
        match cmd {
            "stop" | "quit" => {
                return Resp {
                    out: "wowsir daemon stopped".into(),
                    code: 99,
                }
            }
            "sessions" => {
                let s = self.sessions.lock().await;
                let mut out = vec![];
                for (name, sess) in s.iter() {
                    let url = sess.lock().await.url();
                    out.push(format!("{name}  {url}"));
                }
                return Resp {
                    out: if out.is_empty() {
                        "(no sessions)".into()
                    } else {
                        out.join("\n")
                    },
                    code: 0,
                };
            }
            "close" => {
                if let Some(s) = self.sessions.lock().await.remove(&req.session) {
                    let s = s.lock().await;
                    self.contexts.lock().unwrap().remove(&s.browser_ctx);
                    s.close().await;
                    return Resp {
                        out: format!("closed session {}", req.session),
                        code: 0,
                    };
                }
                return Resp {
                    out: format!("no session {}", req.session),
                    code: 0,
                };
            }
            "auth-reload" => {
                let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
                let mut n = 0;
                for s in sessions {
                    s.lock().await.load_auth_dir().await;
                    n += 1;
                }
                return Resp {
                    out: format!("saved logins reloaded into {n} session(s)"),
                    code: 0,
                };
            }
            "auth-forget" => {
                // args: domains... -- origins...
                let rest = &req.cmd[1..];
                let split = rest.iter().position(|a| a == "--").unwrap_or(rest.len());
                let domains: Vec<String> = rest[..split].to_vec();
                let origins: Vec<String> = rest
                    .get(split + 1..)
                    .map(|o| o.to_vec())
                    .unwrap_or_default();
                let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
                let mut removed = 0;
                for s in &sessions {
                    removed += s.lock().await.forget_site(&domains, &origins).await;
                }
                return Resp {
                    out: format!(
                        "removed {removed} cookies from {} running session(s)",
                        sessions.len()
                    ),
                    code: 0,
                };
            }
            "status" => {
                let b = self.browser.lock().await;
                let n = self.sessions.lock().await.len();
                return Resp {
                    out: format!(
                        "daemon pid {} · browser {} · {} session(s) · {}",
                        std::process::id(),
                        if b.is_some() { "up" } else { "down" },
                        n,
                        if self.headed { "headed" } else { "headless" }
                    ),
                    code: 0,
                };
            }
            _ => {}
        }
        if req.headed && !self.headed {
            return Resp {
                out: "daemon is running headless; run `wowsir stop` then retry with --headed"
                    .into(),
                code: 2,
            };
        }
        let sess = match self.session(&req.session, req.no_auth).await {
            Ok(s) => s,
            Err(e) => {
                return Resp {
                    out: format!("FAIL could not start browser session: {e:#}"),
                    code: 1,
                }
            }
        };
        let mut s = sess.lock().await;
        let ctx = Ctx {
            cwd: PathBuf::from(&req.cwd),
            quiet: req.quiet,
            full_after: req.full_after,
            timeout: req.timeout.map(Duration::from_millis),
        };
        if let Some(batch) = req.batch {
            return run_batch(&mut s, batch, ctx).await;
        }
        let out = s.exec(&req.cmd, &ctx).await;
        Resp {
            out: out.text,
            code: if out.ok { 0 } else { 1 },
        }
    }
}

async fn run_batch(s: &mut Session, steps: Vec<(usize, Vec<String>)>, ctx: Ctx) -> Resp {
    let t0 = Instant::now();
    let total = steps.len();
    let mut lines = vec![];
    let verbose = !ctx.quiet;
    let step_ctx = Ctx {
        cwd: ctx.cwd.clone(),
        quiet: true,
        full_after: false,
        timeout: ctx.timeout,
    };
    for (i, (lineno, step)) in steps.iter().enumerate() {
        let last = i == total - 1;
        let c = if last && verbose { &ctx } else { &step_ctx };
        let out = s.exec(step, c).await;
        if !out.ok {
            lines.push(format!("line {lineno}: {}", out.text));
            lines.push(format!(
                "✗ failed at step {}/{total} · {}ms",
                i + 1,
                t0.elapsed().as_millis()
            ));
            return Resp {
                out: lines.join("\n"),
                code: 1,
            };
        }
        if last && verbose {
            lines.push(out.text);
        } else {
            // headline + any warnings, never snapshots
            let mut it = out.text.lines();
            if let Some(h) = it.next() {
                lines.push(h.to_string());
            }
            for l in
                it.filter(|l| l.starts_with("! ") || l.starts_with("→ ") || l.starts_with("… "))
            {
                lines.push(format!("  {l}"));
            }
        }
    }
    lines.push(format!(
        "✓ {total}/{total} steps passed · {}ms",
        t0.elapsed().as_millis()
    ));
    Resp {
        out: lines.join("\n"),
        code: 0,
    }
}
