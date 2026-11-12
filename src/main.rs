mod browser;
mod cdp;
mod commands;
mod daemon;
mod keys;
mod login;
mod mcp;
mod platform;
mod readonly;
mod session;

use daemon::{Req, Resp};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELP: &str = r#"wowsir — fast headless browser for agents. Every action returns what changed.

NAVIGATE   open <url> · back · forward · reload · url · title
LAYOUT     watch T... · watch (list) · watch off [T]   watched elements report `! moved:` / `! jumped:` after each action
           expect still [T...] · screenshot [PATH] --mark (outlines + coordinates of watched elements) · --mark=T
MOCK       mock [METHOD] PATTERN STATUS|abort [--json X|--body X|--file F] [--delay MS] [--doc] · mock (list) · mock off [PATTERN]
             PATTERN: URL substring or glob; METHOD: POST, PUT|DELETE... (default any); page loads only with --doc
           readonly on|strict|off · readonly allow PATTERN|off · readonly (status)
             blocks POST/PUT/PATCH/DELETE, WebSocket sends and GETs like /logout; strict = a blocked write fails the step
             sites listed in ~/.wowsir/readonly open read-only (`lock` after a site: agents can't turn it off)
TABS       popups and target=_blank pages are followed automatically (! new tab N…)
           tabs · tab N · tab new [URL] · tab close [N]
SEE        snap [-i] [--grep TEXT] [--in TARGET] [--viewport] [--full] [--urls] [--ids] [--all]
             -i interactive only · --grep filter lines · --in scope to element
           text [TARGET] · html [TARGET] · screenshot [PATH] [--full] [--el TARGET]
ACT        click T [--double|--right] [--ctrl|--shift|--alt|--meta|--mod] · click --at X,Y (viewport point: maps, canvas) · tap T|X,Y (touch)
           fill T TEXT · type [T] TEXT · press KEY... (Enter, Tab, Control+a, Mod+Enter)
           select T OPTION... · check T · uncheck T · hover T · focus T · upload T FILE...
           scroll [down|up|top|bottom|T] [PX] · scroll T down|up|PX (inside a scrollable list/panel)
WAIT       wait MS | wait T | wait text TEXT | wait gone T | wait url PART | wait idle
ASSERT     expect text|no-text TEXT · expect url|title PART · expect visible|hidden T
           expect value T VALUE · expect checked|unchecked|mixed T · expect count T N · expect no-errors
           (assertions retry until --timeout, default 5000ms; exit code 1 on failure)
DEBUG      console [--errors] [--all] · net [--failed] [--api] [--grep X] · net --body PATTERN [--full] · net --request PATTERN [--all] (what a request sent) · errors [--clear] · eval JS · eval --file F.js · eval - (stdin) · cookies [clear]
SETUP      viewport W H [--mobile] | viewport mobile|tablet|desktop (mobile: touch + mobile user agent) · block images|fonts|media|css|lite|off
           theme dark|light [reduced-motion] | theme off   (prefers-color-scheme for CSS and matchMedia)
           dialog accept [TEXT]|dismiss (TEXT is typed into prompt())
AUTH       login URL               opens a visible window; a human signs in; cookies saved to ~/.wowsir/auth/
           login --from http://127.0.0.1:9222 DOMAIN   copy one site's cookies from a running debug Chrome
           Saved logins load into every new session automatically (--no-auth for a clean one).
           logout SITE             delete a saved login and wipe that site's cookies/storage from running sessions
           state save FILE · state load FILE   (Playwright-compatible storageState)
TESTS      run FILE...|- [--parallel N]   (one command per line, # comments; stops at first failure)
           record [show|save FILE|clear]   (your session as a replayable test, refs → stable selectors)
SESSIONS   -s NAME / WOWSIR_SESSION=NAME (isolated cookies/storage; parallel-safe) · sessions · close · stop · status

TARGETS (T)  @e12               ref from a snapshot (stable while the element lives)
             button:Sign in     role:name — link:, textbox:, checkbox:, combobox:, h1:, ...
             text=Forgot password
             #id / .class / [data-testid=x]   any CSS selector (pierces open and closed shadow DOM)
                                quote '#id' in the shell: unquoted, # starts a shell comment
             A >> B             B near A: inside it, else the first after it in its section
                                ("h2:Shipping >> checkbox:Gift wrap", "text=Unsaved >> button:Discard")

MCP        wowsir mcp   (stdio MCP server: tools `browser` + `browser_run`, same commands and output as this CLI)
           add to Claude Code: claude mcp add --scope user wowsir -- wowsir mcp
GLOBAL     -q quiet (no diff) · --snap full snapshot after actions · --timeout MS · --headed · --no-auth
After each action wowsir waits for the page to settle (network + DOM quiet), then prints:
  ok <action> · <ms>   then  → new url,  ! console/network errors/dialogs,  · console.warn summary,  ~ diff of the page (+ added, - removed)
"#;

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(|s| s.as_str()) == Some("mcp") {
        mcp::serve();
        return;
    }
    if argv.first().map(|s| s.as_str()) == Some("__daemon") {
        let headed = argv.iter().any(|a| a == "--headed");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        if let Err(e) = rt.block_on(daemon::run(headed)) {
            eprintln!("daemon error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // global flags (anywhere before a literal `--`)
    let mut req = Req {
        session: std::env::var("WOWSIR_SESSION").unwrap_or_else(|_| "default".into()),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        headed: std::env::var("WOWSIR_HEADED").is_ok(),
        no_auth: std::env::var("WOWSIR_NO_AUTH").is_ok(),
        ..Default::default()
    };
    let mut rest = vec![];
    let mut i = 0;
    let mut literal = false;
    while i < argv.len() {
        let a = argv[i].clone();
        if literal {
            rest.push(a);
        } else {
            match a.as_str() {
                "--" => {
                    literal = true;
                    rest.push(a);
                }
                "-s" | "--session" if i + 1 < argv.len() => {
                    req.session = argv[i + 1].clone();
                    i += 1;
                }
                "-q" | "--quiet" => req.quiet = true,
                "--snap" => req.full_after = true,
                "--headed" => req.headed = true,
                "--no-auth" => req.no_auth = true,
                "--timeout" if i + 1 < argv.len() => {
                    req.timeout = argv[i + 1].parse().ok();
                    i += 1;
                }
                _ => rest.push(a),
            }
        }
        i += 1;
    }
    argv = rest;

    let cmd = argv.first().cloned().unwrap_or_else(|| "help".into());
    match cmd.as_str() {
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            return;
        }
        "version" | "-V" | "--version" => {
            println!("wowsir {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        "login" => {
            let a = commands::parse_args(&argv[1..]);
            let result = if let Some(from) = a.flags.get("from") {
                // copy cookies out of a running debug browser (e.g. your dev Chrome on :9222)
                let Some(m) = a.pos.first() else {
                    eprintln!("usage: wowsir login --from http://127.0.0.1:9222 <domain-part, e.g. example.com>");
                    std::process::exit(2);
                };
                let from = if from.is_empty() {
                    "http://127.0.0.1:9222".to_string()
                } else {
                    from.clone()
                };
                let out = a
                    .flags
                    .get("out")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| login::auth_dir().join(format!("{m}.json")));
                login::import_from(&from, m, &out).map(|(n, d)| {
                    format!(
                        "copied {n} cookies for {} from {from} → {}",
                        d.join(", "),
                        out.display()
                    )
                })
            } else {
                let Some(url) = a.pos.first() else {
                    eprintln!("usage: wowsir login <url> [--out FILE]   |   wowsir login --from http://127.0.0.1:9222 <domain-part>");
                    std::process::exit(2);
                };
                let url = if url.contains("://") {
                    url.clone()
                } else {
                    format!("https://{url}")
                };
                let out = a
                    .flags
                    .get("out")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| login::default_state_path(&url));
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(login::run(&url, &out, Duration::from_secs(1800)))
                    .map(|n| format!("saved {n} cookies → {}", out.display()))
            };
            match result {
                Ok(msg) => {
                    println!("{msg}");
                    // push the new login into sessions that are already running
                    let reload = Req {
                        cmd: vec!["auth-reload".into()],
                        session: req.session.clone(),
                        cwd: req.cwd.clone(),
                        ..Default::default()
                    };
                    if let Ok(r) = send(&reload, false) {
                        println!("{}", r.out);
                    }
                    println!("new sessions load it automatically (opt out with --no-auth)");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("FAIL {e:#}");
                    std::process::exit(1);
                }
            }
        }
        "logout" => {
            let Some(what) = argv.get(1) else {
                eprintln!("usage: wowsir logout <site>   (a saved login name like `example` or any URL/host of that site)\nsaved logins: {}",
                    login::saved_logins().iter().map(|l| l.0.clone()).collect::<Vec<_>>().join(", "));
                std::process::exit(2);
            };
            match login::forget(what) {
                Ok((file, domains, origins)) => {
                    println!("deleted saved login {}", file.display());
                    let mut cmd = vec!["auth-forget".to_string()];
                    cmd.extend(domains);
                    cmd.push("--".into());
                    cmd.extend(origins);
                    let r = Req {
                        cmd,
                        session: req.session.clone(),
                        cwd: req.cwd.clone(),
                        ..Default::default()
                    };
                    if let Ok(r) = send(&r, false) {
                        println!("{}", r.out);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("FAIL {e:#}");
                    std::process::exit(1);
                }
            }
        }
        "eval" | "js" if matches!(argv.get(1).map(|s| s.as_str()), Some("-" | "--file" | "-f")) => {
            let code = if argv[1] == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).ok();
                s
            } else {
                let Some(f) = argv.get(2) else {
                    eprintln!("FAIL eval --file needs a path");
                    std::process::exit(2);
                };
                std::fs::read_to_string(f).unwrap_or_else(|e| {
                    eprintln!("FAIL cannot read {f}: {e}");
                    std::process::exit(2);
                })
            };
            req.cmd = vec![cmd.to_string(), code];
        }
        "run" => {
            // run FILE... [--parallel N | -j N]
            let mut files = vec![];
            let mut jobs = 1usize;
            let mut it = argv[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--parallel" | "-j" => {
                        jobs = it
                            .next()
                            .and_then(|n| n.parse().ok())
                            .filter(|n| *n > 0)
                            .unwrap_or_else(|| {
                                eprintln!("FAIL --parallel takes a number, e.g. --parallel 4");
                                std::process::exit(2);
                            })
                    }
                    f => files.push(f.to_string()),
                }
            }
            if files.len() > 1 {
                std::process::exit(run_files(&req, &files, jobs));
            }
            let src = files.first().map(|s| s.as_str()).unwrap_or("-");
            match parse_steps(src) {
                Ok(steps) => {
                    req.cmd = vec!["run".into()];
                    req.batch = Some(steps);
                }
                Err(e) => {
                    eprintln!("FAIL {e}");
                    std::process::exit(2);
                }
            }
        }
        _ => req.cmd = argv,
    }

    let stopping = cmd == "stop" || cmd == "quit";
    let resp = match send(&req, !stopping) {
        Ok(r) => r,
        Err(e) => {
            if stopping {
                println!("wowsir daemon not running");
                return;
            }
            eprintln!("FAIL {e}");
            std::process::exit(2);
        }
    };
    if !resp.out.is_empty() {
        println!("{}", resp.out);
    }
    if resp.code == 99 {
        // wait for the old daemon to release its lock so the next command starts fresh
        let lock = daemon::dir().join("daemon.lock");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if !lock.exists() || platform::try_lock(&lock).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    std::process::exit(if resp.code == 99 { 0 } else { resp.code });
}

/// Read a .wowsir file (or stdin for "-") into numbered steps.
fn parse_steps(src: &str) -> Result<Vec<(usize, Vec<String>)>, String> {
    let text = if src == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).ok();
        s
    } else {
        std::fs::read_to_string(src).map_err(|e| format!("cannot read {src}: {e}"))?
    };
    let mut steps = vec![];
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("wowsir ").unwrap_or(l);
        if let Some(w) = commands::eval_line(l) {
            steps.push((n + 1, w));
            continue;
        }
        match commands::split_line(l) {
            Ok(w) if !w.is_empty() => steps.push((n + 1, w)),
            Ok(_) => {}
            Err(e) => return Err(format!("{src} line {}: {e}", n + 1)),
        }
    }
    if steps.is_empty() {
        return Err(format!("no steps in {src}"));
    }
    Ok(steps)
}

/// Several test files: each in its own fresh session, `jobs` at a time.
/// Output is printed per file in the given order. Returns the exit code.
fn run_files(base: &Req, files: &[String], jobs: usize) -> i32 {
    let t0 = Instant::now();
    // start the daemon once, not from N threads at the same time
    let mut ping = base.clone();
    ping.cmd = vec!["status".into()];
    if let Err(e) = send(&ping, true) {
        eprintln!("FAIL {e}");
        return 2;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<(bool, String)>>> =
        files.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|sc| {
        for _ in 0..jobs.min(files.len()) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(file) = files.get(i) else { break };
                let stem = std::path::Path::new(file)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let session = format!("{}-run{i}-{stem}", base.session);
                let r = match parse_steps(file) {
                    Err(e) => (false, format!("FAIL {e}")),
                    Ok(steps) => {
                        let mut req = base.clone();
                        req.session = session.clone();
                        req.cmd = vec!["run".into()];
                        req.batch = Some(steps);
                        let r = match send(&req, true) {
                            Ok(resp) => (resp.code == 0, resp.out),
                            Err(e) => (false, format!("FAIL {e}")),
                        };
                        let mut close = base.clone();
                        close.session = session;
                        close.cmd = vec!["close".into()];
                        let _ = send(&close, false);
                        r
                    }
                };
                *results[i].lock().unwrap() = Some(r);
            });
        }
    });
    let mut passed = 0;
    for (file, r) in files.iter().zip(&results) {
        let (ok, out) = r
            .lock()
            .unwrap()
            .take()
            .unwrap_or((false, "FAIL not run".into()));
        println!("== {file}\n{out}");
        if ok {
            passed += 1;
        }
    }
    let n = files.len();
    let ms = t0.elapsed().as_millis();
    if passed == n {
        println!("✓ {n}/{n} files passed · {ms}ms ({jobs} at a time)");
        0
    } else {
        println!("✗ {passed}/{n} files passed · {ms}ms ({jobs} at a time)");
        1
    }
}

fn send(req: &Req, autostart: bool) -> Result<Resp, String> {
    let mut stream = match platform::connect(&daemon::endpoint()) {
        Ok(s) => s,
        Err(_) if autostart => start_daemon(req.headed)?,
        Err(e) => return Err(e.to_string()),
    };
    let mut req = req.clone();
    req.build = daemon::build_id();
    let mut body = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    body.push(b'\n');
    stream.write_all(&body).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("daemon connection lost: {e}"))?;
    if line.is_empty() {
        return Err(
            "daemon closed the connection (crashed?). See daemon.log in ".to_string()
                + &daemon::dir().display().to_string(),
        );
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

fn start_daemon(headed: bool) -> Result<platform::ClientStream, String> {
    let dir = daemon::dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("__daemon");
    if headed {
        cmd.arg("--headed");
    }
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    platform::detach(&mut cmd);
    cmd.spawn()
        .map_err(|e| format!("could not start daemon: {e}"))?;
    let start = Instant::now();
    loop {
        if let Ok(s) = platform::connect(&daemon::endpoint()) {
            return Ok(s);
        }
        if start.elapsed() > Duration::from_secs(15) {
            return Err(format!(
                "daemon did not start; see {}",
                dir.join("daemon.log").display()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
