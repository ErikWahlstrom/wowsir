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
    let pw = if cfg!(target_os = "macos") {
        home().join("Library/Caches/ms-playwright")
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("ms-playwright")
    } else {
        home().join(".cache/ms-playwright")
    };
    // chrome-headless-shell: the old headless mode, no browser UI layer → fastest startup.
    if !headed {
        if let Some(d) = newest(&pw, "chromium_headless_shell-") {
            for c in [
                "chrome-mac/headless_shell",
                "chrome-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-linux/headless_shell",
                "chrome-headless-shell-linux64/chrome-headless-shell",
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
    if let Some(d) = newest(&pw, "chromium-") {
        cands.push(d.join("chrome-mac/Chromium.app/Contents/MacOS/Chromium"));
        cands.push(d.join("chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium"));
        cands.push(d.join("chrome-linux/chrome"));
        cands.push(d.join("chrome-linux64/chrome"));
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
    bail!("no Chromium found. Install one (`npx playwright install chromium-headless-shell`) or set WOWSIR_CHROME=/path/to/chrome")
}

pub fn launch(headed: bool) -> Result<Browser> {
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
    let no_sandbox = std::env::var("WOWSIR_NO_SANDBOX")
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (child, rx, tx) = crate::platform::spawn_with_cdp_pipe(&mut cmd)
        .with_context(|| format!("launching {}", bin.display()))?;
    let cdp = Cdp::new(rx, tx);
    Ok(Browser {
        cdp,
        child,
        profile,
    })
}
