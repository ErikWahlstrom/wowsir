//! Locate and launch Chromium with a CDP pipe (fd 3/4 on Unix, inherited handles on Windows).

use crate::cdp::Cdp;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

pub struct Browser {
    pub cdp: Arc<Cdp>,
    pub child: Child,
    pub profile: PathBuf,
    /// launched with --no-sandbox
    pub no_sandbox: bool,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn home() -> PathBuf {
    crate::platform::home_dir()
}

/// Newest directory matching `prefix-<N>` inside `dir`.
fn newest(dir: &PathBuf, prefix: &str) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix(prefix) {
            if let Ok(n) = rest.parse::<u32>() {
                if best.as_ref().is_none_or(|(b, _)| n > *b) {
                    best = Some((n, e.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Returns (binary, is_headless_shell).
pub fn find_chrome(headed: bool) -> Result<(PathBuf, bool)> {
    if let Some(p) = std::env::var_os("WOWSIR_CHROME") {
        let p = PathBuf::from(p);
        let shell = p.to_string_lossy().contains("headless_shell");
        return Ok((p, shell));
    }
    let dirs = playwright_dirs();
    // chrome-headless-shell: the old headless mode, no browser UI layer → fastest startup.
    if !headed {
        for d in dirs
            .iter()
            .filter_map(|pw| newest(pw, "chromium_headless_shell-"))
        {
            for c in [
                "chrome-mac/headless_shell",
                "chrome-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-linux/headless_shell",
                "chrome-headless-shell-linux64/chrome-headless-shell",
                "chrome-headless-shell-linux-arm64/chrome-headless-shell",
                "chrome-win/headless_shell.exe",
                "chrome-headless-shell-win64/chrome-headless-shell.exe",
            ] {
                let p = d.join(c);
                if p.exists() {
                    return Ok((p, true));
                }
            }
        }
    }
    let mut cands: Vec<PathBuf> = vec![];
    for d in dirs.iter().filter_map(|pw| newest(pw, "chromium-")) {
        cands.push(d.join("chrome-mac/Chromium.app/Contents/MacOS/Chromium"));
        cands.push(d.join("chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium"));
        cands.push(d.join("chrome-linux/chrome"));
        cands.push(d.join("chrome-linux64/chrome"));
        cands.push(d.join("chrome-linux-arm64/chrome"));
        cands.push(d.join("chrome-win/chrome.exe"));
        cands.push(d.join("chrome-win64/chrome.exe"));
    }
    // Windows: per-machine and per-user installs of Chrome, then Edge (always present)
    for (var, rel) in [
        ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
        ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
        ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
        (
            "ProgramFiles(x86)",
            r"Microsoft\Edge\Application\msedge.exe",
        ),
        ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
    ] {
        if let Some(base) = std::env::var_os(var) {
            cands.push(PathBuf::from(base).join(rel));
        }
    }
    for p in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ] {
        cands.push(PathBuf::from(p));
    }
    for c in cands {
        if c.exists() {
            return Ok((c, false));
        }
    }
    bail!("no Chromium found (looked in {}). Install one (`npx playwright install chromium-headless-shell`), or set PLAYWRIGHT_BROWSERS_PATH or WOWSIR_CHROME=/path/to/chrome",
        dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "))
}

/// Where Playwright keeps its browsers: $PLAYWRIGHT_BROWSERS_PATH if set (`0` means
/// node_modules/playwright-core/.local-browsers, found from the working directory up),
/// then the per-user default.
fn playwright_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    match std::env::var_os("PLAYWRIGHT_BROWSERS_PATH") {
        Some(v) if v == "0" => {
            if let Ok(cwd) = std::env::current_dir() {
                for a in cwd.ancestors() {
                    let d = a.join("node_modules/playwright-core/.local-browsers");
                    if d.is_dir() {
                        dirs.push(d);
                        break;
                    }
                }
            }
        }
        Some(v) if !v.is_empty() => {
            let p = PathBuf::from(v);
            // relative paths are relative to where Playwright ran: try the working directory
            dirs.push(if p.is_relative() {
                std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
            } else {
                p
            });
        }
        _ => {}
    }
    dirs.push(if cfg!(target_os = "macos") {
        home().join("Library/Caches/ms-playwright")
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("ms-playwright")
    } else {
        home().join(".cache/ms-playwright")
    });
    dirs
}

/// Launch Chromium and check that it answers. If Chrome's own sandbox can't start
/// (containers, bwrap and other sandboxes), retry once without it, as Playwright does.
pub async fn start(headed: bool) -> Result<Browser> {
    let mut br = launch(headed, false)?;
    let Err(e) = br
        .cdp
        .call(None, "Browser.getVersion", serde_json::json!({}))
        .await
    else {
        return Ok(br);
    };
    let (status, log) = br.died();
    let sandboxed = !br.no_sandbox;
    drop(br);
    if sandboxed && sandbox_failure(&log) {
        let mut br = launch(headed, true)?;
        match br
            .cdp
            .call(None, "Browser.getVersion", serde_json::json!({}))
            .await
        {
            Ok(_) => {
                eprintln!("Chrome's sandbox could not start here; running it with --no-sandbox (set WOWSIR_NO_SANDBOX=1 to skip the first try)");
                return Ok(br);
            }
            Err(e) => {
                let (status, log) = br.died();
                bail!(startup_error(e, status, &log, false));
            }
        }
    }
    bail!(startup_error(e, status, &log, sandboxed))
}

fn sandbox_failure(log: &str) -> bool {
    let l = log.to_lowercase();
    l.contains("sandbox") || l.contains("namespace") || l.contains("zygote")
}

fn startup_error(e: anyhow::Error, status: String, log: &str, sandboxed: bool) -> String {
    let tail: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = tail[tail.len().saturating_sub(6)..].join("\n  ");
    let mut m = format!("Chrome did not start ({e}; {status})");
    if !tail.is_empty() {
        m += &format!(". Its last output:\n  {tail}");
    }
    if sandboxed {
        m += "\nInside a container or sandbox (Docker, bwrap) Chrome's own sandbox often can't start: try WOWSIR_NO_SANDBOX=1";
    }
    m
}

impl Browser {
    /// After a failed start: how Chrome exited, and what it printed.
    fn died(&mut self) -> (String, String) {
        let mut status = "still running".to_string();
        for _ in 0..20 {
            if let Ok(Some(s)) = self.child.try_wait() {
                status = format!("exited: {s}");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let log =
            std::fs::read_to_string(self.profile.join("chrome-stderr.log")).unwrap_or_default();
        (status, log)
    }
}

pub fn launch(headed: bool, force_no_sandbox: bool) -> Result<Browser> {
    let (bin, _shell) = find_chrome(headed)?;
    let profile = std::env::temp_dir().join(format!("wowsir-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile)?;

    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-component-extensions-with-background-pages",
            "--disable-background-networking",
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
            "--disable-ipc-flooding-protection",
            "--disable-sync",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-domain-reliability",
            "--disable-client-side-phishing-detection",
            "--disable-hang-monitor",
            "--disable-popup-blocking",
            "--disable-prompt-on-repost",
            "--disable-breakpad",
            "--metrics-recording-only",
            "--password-store=basic",
            "--use-mock-keychain",
            "--mute-audio",
            "--hide-scrollbars",
            "--force-color-profile=srgb",
            "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,AcceptCHFrame,AutoExpandDetailsElement,CertificateTransparencyComponentUpdater,AvoidUnnecessaryBeforeUnloadCheckSync,PaintHolding,LensOverlay,PrivacySandboxSettings4,ThirdPartyStoragePartitioning",
            "--enable-features=NetworkServiceInProcess2",
            "--window-size=1280,800",
        ]);
    // Chrome's sandbox can't start as root (containers) or under some CI kernels: opt out there
    let no_sandbox = force_no_sandbox
        || std::env::var("WOWSIR_NO_SANDBOX")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
        || (cfg!(target_os = "linux") && crate::platform::is_root());
    if no_sandbox {
        cmd.arg("--no-sandbox");
    }
    if !headed
        && !bin.to_string_lossy().contains("headless_shell")
        && !bin.to_string_lossy().contains("chrome-headless-shell")
    {
        cmd.arg("--headless=new");
    }
    cmd.arg("about:blank");
    // Chrome's stderr goes to a file in its profile, read back if it fails to start
    let stderr = std::fs::File::create(profile.join("chrome-stderr.log"))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    let (child, rx, tx) = crate::platform::spawn_with_cdp_pipe(&mut cmd)
        .with_context(|| format!("launching {}", bin.display()))?;
    let cdp = Cdp::new(rx, tx);
    Ok(Browser {
        cdp,
        child,
        profile,
        no_sandbox,
    })
}
