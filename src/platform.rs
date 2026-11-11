//! Everything that differs between Unix (macOS/Linux) and Windows: where files
//! live, the CLI ↔ daemon channel, detaching the daemon, single-instance locks,
//! and handing Chrome its CDP pipe.
//!
//! Unix: a unix socket, setsid, flock, and the pipe on fd 3/4.
//! Windows: a named pipe, DETACHED_PROCESS, an unshared lock file, and the pipe
//! as inherited handles named in `--remote-debugging-io-pipes`.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::{AsyncRead, AsyncWrite};

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into())
}

/// Running as root (Chrome's sandbox refuses to start there).
pub fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        false
    }
}

/// Default directory for the daemon's socket, lock and log.
pub fn runtime_dir() -> PathBuf {
    #[cfg(unix)]
    {
        // fixed path (not $TMPDIR) so every caller finds the same daemon
        PathBuf::from(format!("/tmp/wowsir-{}", unsafe { libc::getuid() }))
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("wowsir")
    }
}

/// Saved logins are readable only by the user.
pub fn make_private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        let _ = path; // files under the user profile are private to the user by default
        Ok(())
    }
}

/// Open `path` and take an exclusive, non-blocking lock on it. None if another
/// process holds it. The lock lasts as long as the returned File.
pub fn try_lock(path: &Path) -> Option<File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // share_mode(0): nobody else can open it while we hold it
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .share_mode(0)
            .open(path)
            .ok()
    }
}

/// Start `cmd` in the background, detached from the terminal that launched it.
pub fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

// ------------------------------------------------------------------ CLI ↔ daemon

/// Where the daemon listens: a unix socket path, or a named pipe name.
pub fn endpoint(dir: &Path) -> String {
    #[cfg(unix)]
    {
        let p = dir.join("wowsir.sock");
        // unix socket paths max out around 104 bytes: fall back to a short, stable path
        if p.as_os_str().len() > 100 {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            p.hash(&mut h);
            return format!(
                "/tmp/wowsir-{}-{:x}.sock",
                unsafe { libc::getuid() },
                h.finish()
            );
        }
        p.to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        // one pipe per daemon directory (so WOWSIR_HOME isolates, as on Unix)
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        dir.hash(&mut h);
        format!(r"\\.\pipe\wowsir-{:x}", h.finish())
    }
}

/// Client side of the daemon channel (blocking).
#[cfg(unix)]
pub type ClientStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type ClientStream = File;

pub fn connect(endpoint: &str) -> io::Result<ClientStream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(endpoint)
    }
    #[cfg(windows)]
    {
        // all pipe instances busy (ERROR_PIPE_BUSY = 231): the server makes a new one right away
        let start = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(endpoint)
            {
                Err(e) if e.raw_os_error() == Some(231) && start.elapsed().as_secs() < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                r => return r,
            }
        }
    }
}

/// Remove a stale endpoint before binding / after shutdown (no-op for named pipes).
pub fn remove_endpoint(endpoint: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(endpoint);
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
    }
}

pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Conn for T {}

/// Server side of the daemon channel.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    name: String,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl Listener {
    pub fn bind(endpoint: &str) -> io::Result<Self> {
        #[cfg(unix)]
        {
            remove_endpoint(endpoint);
            Ok(Listener {
                inner: tokio::net::UnixListener::bind(endpoint)?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(endpoint)?;
            Ok(Listener {
                name: endpoint.to_string(),
                next,
            })
        }
    }

    pub async fn accept(&mut self) -> io::Result<Box<dyn Conn>> {
        #[cfg(unix)]
        {
            let (s, _) = self.inner.accept().await?;
            Ok(Box::new(s))
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            self.next.connect().await?;
            // a fresh instance for the next client before handing this one off
            let fresh = ServerOptions::new().create(&self.name)?;
            Ok(Box::new(std::mem::replace(&mut self.next, fresh)))
        }
    }
}

// ------------------------------------------------------------------ Chrome's CDP pipe

pub type PipeRead = Box<dyn AsyncRead + Unpin + Send>;
pub type PipeWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Spawn Chrome (already configured with its other args) with a CDP pipe:
/// it reads commands from one pipe and writes responses to the other.
pub fn spawn_with_cdp_pipe(
    cmd: &mut Command,
) -> io::Result<(std::process::Child, PipeRead, PipeWrite)> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        use tokio::net::unix::pipe;
        let (to_browser_r, to_browser_w) = std::io::pipe()?;
        let (from_browser_r, from_browser_w) = std::io::pipe()?;
        let (a, b) = (to_browser_r.as_raw_fd(), from_browser_w.as_raw_fd());
        cmd.arg("--remote-debugging-pipe");
        unsafe {
            cmd.pre_exec(move || {
                // Move to high fds first so dup2 onto 3/4 can't clobber a source fd.
                let a = libc::fcntl(a, libc::F_DUPFD, 10);
                let b = libc::fcntl(b, libc::F_DUPFD, 10);
                if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop((to_browser_r, from_browser_w)); // the browser's ends live in the child now
        let tx = pipe::Sender::from_owned_fd(OwnedFd::from(to_browser_w))?;
        let rx = pipe::Receiver::from_owned_fd(OwnedFd::from(from_browser_r))?;
        Ok((child, Box::new(rx), Box::new(tx)))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, OwnedHandle};
        use std::os::windows::process::CommandExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn SetHandleInformation(h: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 1;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let (to_browser_r, to_browser_w) = std::io::pipe()?;
        let (from_browser_r, from_browser_w) = std::io::pipe()?;
        // only the browser's ends are inherited; Chrome takes them as raw handle values
        for h in [to_browser_r.as_raw_handle(), from_browser_w.as_raw_handle()] {
            if unsafe { SetHandleInformation(h as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        cmd.arg("--remote-debugging-pipe").arg(format!(
            "--remote-debugging-io-pipes={},{}",
            to_browser_r.as_raw_handle() as usize,
            from_browser_w.as_raw_handle() as usize
        ));
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        let child = cmd.spawn()?;
        drop((to_browser_r, from_browser_w));
        // anonymous pipes can't do overlapped I/O: tokio's File runs reads/writes on worker threads
        let tx = tokio::fs::File::from_std(File::from(OwnedHandle::from(to_browser_w)));
        let rx = tokio::fs::File::from_std(File::from(OwnedHandle::from(from_browser_r)));
        Ok((child, Box::new(rx), Box::new(tx)))
    }
}
