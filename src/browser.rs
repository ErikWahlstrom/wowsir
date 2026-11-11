//! Locate and launch Chromium with a CDP pipe attached on fd 3/4.

use crate::cdp::Cdp;
use anyhow::{bail, Context, Result};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tokio::net::unix::pipe;

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
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

/// Newest directory matching `prefix-<N>` inside `dir`.
fn newest(dir: &PathBuf, prefix: &str) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix(prefix) {
            if let Ok(n) = rest.parse::<u32>() {
                if best.as_ref().map_or(true, |(b, _)| n > *b) {
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

fn mkpipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("pipe() failed: {}", std::io::Error::last_os_error());
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((fds[0], fds[1]))
}

pub fn launch(headed: bool) -> Result<Browser> {
    let (bin, _shell) = find_chrome(headed)?;
    let profile = std::env::temp_dir().join(format!("wowsir-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile)?;

    // browser reads commands from fd 3, writes responses to fd 4
    let (to_browser_r, to_browser_w) = mkpipe()?;
    let (from_browser_r, from_browser_w) = mkpipe()?;

    let mut cmd = Command::new(&bin);
    cmd.arg("--remote-debugging-pipe")
        .arg(format!("--user-data-dir={}", profile.display()))
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
    let no_sandbox = std::env::var("WOWSIR_NO_SANDBOX").map(|v| v != "0" && !v.is_empty()).unwrap_or(false)
        || (cfg!(target_os = "linux") && unsafe { libc::geteuid() } == 0);
    if no_sandbox {
        cmd.arg("--no-sandbox");
    }
    if !headed {
        if !bin.to_string_lossy().contains("headless_shell")
            && !bin.to_string_lossy().contains("chrome-headless-shell")
        {
            cmd.arg("--headless=new");
        }
    }
    cmd.arg("about:blank");
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            // Move to high fds first so dup2 onto 3/4 can't clobber a source fd.
            let a = libc::fcntl(to_browser_r, libc::F_DUPFD, 10);
            let b = libc::fcntl(from_browser_w, libc::F_DUPFD, 10);
            if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("launching {}", bin.display()))?;
    unsafe {
        libc::close(to_browser_r);
        libc::close(from_browser_w);
    }
    let tx = pipe::Sender::from_owned_fd(unsafe { OwnedFd::from_raw_fd(to_browser_w) })?;
    let rx = pipe::Receiver::from_owned_fd(unsafe { OwnedFd::from_raw_fd(from_browser_r) })?;
    let cdp = Cdp::new(rx, tx);
    Ok(Browser { cdp, child, profile })
}
