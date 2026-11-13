//! Child-process runner that can always be stopped.
//!
//! The tools spawn their own children: yt-dlp starts FFmpeg, and the PyInstaller `yt-dlp.exe`
//! is itself a launcher plus a Python child. Killing only the direct child leaves the rest running.
//! They also keep our stderr pipe open. So every run gets its own process tree:
//! - Windows: a Job Object with `KILL_ON_JOB_CLOSE`. The tree dies on cancel, on return and if
//!   this app exits or crashes.
//! - Unix: a fresh process group, killed with `kill -KILL -- -pgid`.
//!
//! stderr is streamed line by line (`\r` counts as a line break, so FFmpeg's progress lines arrive
//! live). Cancellation is polled every 50 ms.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use psx2psp_core::Monitor;

use crate::MediaError;
use crate::tools::command;

const POLL: Duration = Duration::from_millis(50);
/// Lines of stderr kept for the error message.
const TAIL: usize = 6;

/// Runs `program` to completion. Each stderr line goes to `on_line`. The process tree is killed
/// when `mon` reports cancellation, or when it prints nothing for `idle`.
pub fn run_watched(
    program: &Path,
    args: &[&OsStr],
    mon: &dyn Monitor,
    on_line: &mut dyn FnMut(&str),
    idle: Option<Duration>,
) -> Result<Output, MediaError> {
    let tool = program.file_stem().map_or_else(|| program.display().to_string(), |s| s.to_string_lossy().into_owned());
    if mon.cancelled() {
        return Err(MediaError::Cancelled);
    }
    let mut cmd = command(program);
    cmd.args(args);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| MediaError::Io(program.display().to_string(), e))?;
    let tree = Tree::attach(&child);

    let (out_tx, out_rx) = mpsc::channel();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        let _ = out_tx.send(v);
    });
    let (line_tx, line_rx) = mpsc::channel();
    let stderr = child.stderr.take().expect("stderr is piped");
    std::thread::spawn(move || split_lines(stderr, &line_tx));

    let mut tail: VecDeque<String> = VecDeque::new();
    let mut all = String::new();
    let mut take = |l: String, on_line: &mut dyn FnMut(&str)| {
        on_line(&l);
        if all.len() < 64 * 1024 {
            all.push_str(&l);
            all.push('\n');
        }
        if !is_progress(&l) {
            if tail.len() == TAIL {
                tail.pop_front();
            }
            tail.push_back(l);
        }
    };

    let mut last_output = Instant::now();
    let mut stderr_open = true;
    let status = loop {
        if stderr_open {
            match line_rx.recv_timeout(POLL) {
                Ok(l) => {
                    last_output = Instant::now();
                    take(l, on_line);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => stderr_open = false,
            }
        } else {
            std::thread::sleep(POLL);
        }
        if mon.cancelled() {
            tree.kill(&mut child);
            return Err(MediaError::Cancelled);
        }
        if let Some(limit) = idle
            && last_output.elapsed() > limit
        {
            tree.kill(&mut child);
            return Err(MediaError::Tool { tool, detail: format!("no response for {} s, stopped", limit.as_secs()) });
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                tree.kill(&mut child);
                return Err(MediaError::Io(program.display().to_string(), e));
            }
        }
    };
    // Kills leftover grandchildren, which closes the pipes they inherited.
    drop(tree);
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Ok(l) = line_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        take(l, on_line);
    }
    let stdout = out_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    if !status.success() {
        let detail = if tail.is_empty() { format!("exit status {status}") } else { Vec::from(tail).join(" | ") };
        return Err(MediaError::Tool { tool, detail });
    }
    Ok(Output { status, stdout, stderr: all.into_bytes() })
}

/// Forwards `r` as lines, splitting on `\n` and `\r`. Blank lines are dropped.
fn split_lines(mut r: impl Read, tx: &mpsc::Sender<String>) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let flush = |line: &mut Vec<u8>| {
        let s = String::from_utf8_lossy(line).trim().to_string();
        line.clear();
        s.is_empty() || tx.send(s).is_ok()
    };
    while let Ok(n) = r.read(&mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !flush(&mut line) {
                    return;
                }
            } else {
                line.push(b);
            }
        }
    }
    flush(&mut line);
}

/// FFmpeg progress noise (`-progress` key=value lines and `frame= … time= …` stats).
fn is_progress(l: &str) -> bool {
    (l.contains('=') && !l.contains(' ')) || l.starts_with("frame=") || l.starts_with("size=")
}

/// Position in seconds reported by an FFmpeg progress line, if this is one.
/// Understands `-progress` output (`out_time_us=…`) and the classic stats line (`… time=00:01:02.50 …`).
pub fn progress_secs(l: &str) -> Option<f64> {
    if let Some(us) = l.strip_prefix("out_time_us=") {
        return us.trim().parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| v / 1e6);
    }
    let rest = &l[l.find("time=")? + 5..];
    crate::tools::parse_hms(rest.split_whitespace().next()?)
}

/// The process tree of one run.
struct Tree {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    pgid: u32,
}

impl Tree {
    #[cfg(windows)]
    fn attach(child: &Child) -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        // SAFETY: plain Win32 calls on a fresh job handle and on the live child's process handle.
        // The info struct is fully initialised (zeroed POD plus one flag) and outlives the call.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if !job.is_null() {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                AssignProcessToJobObject(job, child.as_raw_handle());
            }
            Self { job }
        }
    }

    #[cfg(unix)]
    fn attach(child: &Child) -> Self {
        Self { pgid: child.id() }
    }

    #[cfg(not(any(windows, unix)))]
    fn attach(_: &Child) -> Self {
        Self {}
    }

    /// Kills the whole tree and reaps the direct child.
    fn kill(&self, child: &mut Child) {
        #[cfg(windows)]
        if !self.job.is_null() {
            // SAFETY: `job` is a valid job handle owned by `self`.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
        #[cfg(unix)]
        let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{}", self.pgid)]).status();
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.job.is_null() {
            // SAFETY: closes the handle created in `attach`. KILL_ON_JOB_CLOSE ends any survivors.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines() {
        assert_eq!(progress_secs("out_time_us=12500000"), Some(12.5));
        assert_eq!(progress_secs("frame=  10 fps=0.0 q=-1.0 size=  256KiB time=00:00:03.50 bitrate=599.2kbits/s speed=6.9x"), Some(3.5));
        assert_eq!(progress_secs("size=N/A time=N/A bitrate=N/A"), None);
        assert_eq!(progress_secs("[youtube] abc: Downloading webpage"), None);
        assert!(is_progress("out_time_us=1") && is_progress("frame=  10 fps=0.0") && !is_progress("Error opening input"));
    }

    struct Cancelled;
    impl Monitor for Cancelled {
        fn cancelled(&self) -> bool {
            true
        }
    }

    /// A long-running child is stopped within a poll interval of cancellation.
    #[test]
    fn cancel_kills_running_child() {
        #[cfg(windows)]
        let (prog, args): (&str, Vec<&OsStr>) = ("powershell", vec!["-NoProfile".as_ref(), "-Command".as_ref(), "Start-Sleep 30".as_ref()]);
        #[cfg(not(windows))]
        let (prog, args): (&str, Vec<&OsStr>) = ("sleep", vec!["30".as_ref()]);
        let Some(p) = crate::tools::which(prog) else { return };
        let flag = std::sync::atomic::AtomicBool::new(false);
        struct Flag<'a>(&'a std::sync::atomic::AtomicBool);
        impl Monitor for Flag<'_> {
            fn cancelled(&self) -> bool {
                self.0.load(std::sync::atomic::Ordering::Relaxed)
            }
        }
        let t0 = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let r = run_watched(&p, &args, &Flag(&flag), &mut |_| {}, None);
            assert!(matches!(r, Err(MediaError::Cancelled)), "{r:?}");
        });
        assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
        assert!(matches!(run_watched(&p, &args, &Cancelled, &mut |_| {}, None), Err(MediaError::Cancelled)));
    }
}
