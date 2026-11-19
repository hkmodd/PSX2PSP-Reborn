//! Child-process runner that can always be stopped.
//!
//! The tools spawn their own children: yt-dlp starts FFmpeg, and the PyInstaller `yt-dlp.exe`
//! is itself a launcher plus a Python child. Killing only the direct child leaves the rest running.
//! They also keep our pipes open. So every run gets its own process tree:
//! - Windows: a Job Object with `KILL_ON_JOB_CLOSE`. The child starts suspended and only runs once
//!   it is in the job, so nothing it spawns can escape. The tree dies on cancel, on return and if
//!   this app exits or crashes.
//! - Unix: a fresh process group, killed with `kill -KILL -- -pgid` on cancel and after every run.
//!
//! stderr is streamed line by line (`\r` counts as a line break, so FFmpeg's progress lines arrive
//! live). Output on either pipe counts as a sign of life for the idle limit. Cancellation is
//! polled every 50 ms, including while the last of stdout is collected.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ExitStatus, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use psx2psp_core::Monitor;

use crate::MediaError;
use crate::tools::command;

const POLL: Duration = Duration::from_millis(50);
/// Lines of stderr kept for the error message.
const TAIL: usize = 6;
/// stderr kept whole in [`Output::stderr`].
const STDERR_KEEP: usize = 64 * 1024;

/// Runs `program` to completion and fails unless it exits with success. Each stderr line goes to
/// `on_line`. The process tree is killed when `mon` reports cancellation, or when it prints
/// nothing on stdout or stderr for `idle`.
pub fn run_watched(
    program: &Path,
    args: &[&OsStr],
    mon: &dyn Monitor,
    on_line: &mut dyn FnMut(&str),
    idle: Option<Duration>,
) -> Result<Output, MediaError> {
    let (out, tail) = run_tree(program, args, mon, on_line, idle)?;
    if !out.status.success() {
        let detail = if tail.is_empty() { format!("exit status {}", out.status) } else { Vec::from(tail).join(" | ") };
        return Err(MediaError::Tool { tool: tool_name(program), detail });
    }
    Ok(out)
}

/// Like [`run_watched`], but any exit status is returned: for tools whose useful output comes
/// with a failure code (`ffmpeg -i <file>` prints the stream info, then exits 1).
pub fn run_any(program: &Path, args: &[&OsStr], mon: &dyn Monitor, idle: Option<Duration>) -> Result<Output, MediaError> {
    run_tree(program, args, mon, &mut |_| {}, idle).map(|(out, _)| out)
}

fn tool_name(program: &Path) -> String {
    program.file_stem().map_or_else(|| program.display().to_string(), |s| s.to_string_lossy().into_owned())
}

/// The run itself: the output (whatever the exit status) and the last meaningful stderr lines.
fn run_tree(
    program: &Path,
    args: &[&OsStr],
    mon: &dyn Monitor,
    on_line: &mut dyn FnMut(&str),
    idle: Option<Duration>,
) -> Result<(Output, VecDeque<String>), MediaError> {
    let io_err = |e| MediaError::Io(program.display().to_string(), e);
    if mon.cancelled() {
        return Err(MediaError::Cancelled);
    }
    let mut cmd = command(program);
    cmd.args(args);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, crate::tools::CREATE_NO_WINDOW | CREATE_SUSPENDED);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(io_err)?;
    let tree = match Tree::attach(&child) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io_err(e));
        }
    };

    // stdout is collected whole; every chunk also bumps `stdout_seen` for the idle limit.
    let stdout_seen = Arc::new(AtomicU64::new(0));
    let (out_tx, out_rx) = mpsc::channel();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let seen = Arc::clone(&stdout_seen);
    std::thread::spawn(move || {
        let (mut v, mut buf) = (Vec::new(), [0u8; 8192]);
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            v.extend_from_slice(&buf[..n]);
            seen.fetch_add(n as u64, Ordering::Relaxed);
        }
        let _ = out_tx.send(v);
    });
    let (line_tx, line_rx) = mpsc::channel();
    let stderr = child.stderr.take().expect("stderr is piped");
    std::thread::spawn(move || split_lines(stderr, &line_tx));

    let mut tail: VecDeque<String> = VecDeque::new();
    let mut all = String::new();
    let mut take = |l: String, on_line: &mut dyn FnMut(&str)| {
        on_line(&l);
        if all.len() < STDERR_KEEP {
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
    let mut last_seen = 0;
    let mut stderr_open = true;
    let status: ExitStatus = loop {
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
        let seen = stdout_seen.load(Ordering::Relaxed);
        if seen != last_seen {
            (last_seen, last_output) = (seen, Instant::now());
        }
        if mon.cancelled() {
            tree.kill(&mut child);
            return Err(MediaError::Cancelled);
        }
        if let Some(limit) = idle
            && last_output.elapsed() > limit
        {
            tree.kill(&mut child);
            return Err(MediaError::Tool { tool: tool_name(program), detail: format!("no response for {} s, stopped", limit.as_secs()) });
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                tree.kill(&mut child);
                return Err(io_err(e));
            }
        }
    };
    // Kills leftover grandchildren, which closes the pipes they inherited.
    drop(tree);
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Ok(l) = line_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        take(l, on_line);
    }
    // All of stdout: the reader ends at EOF, which the tree kill above guarantees.
    let stdout = loop {
        match out_rx.recv_timeout(POLL) {
            Ok(v) => break v,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Vec::new(),
            Err(mpsc::RecvTimeoutError::Timeout) if mon.cancelled() => return Err(MediaError::Cancelled),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    Ok((Output { status, stdout, stderr: all.into_bytes() }, tail))
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

#[cfg(windows)]
const CREATE_SUSPENDED: u32 = windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

/// The process tree of one run.
struct Tree {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    pgid: u32,
}

impl Tree {
    /// Puts the suspended child in a fresh kill-on-close job, then lets it run.
    #[cfg(windows)]
    fn attach(child: &Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        // SAFETY: plain Win32 calls on a fresh job handle and on the live child's process handle.
        // The info struct is fully initialised (zeroed POD plus one flag) and outlives the call.
        // Each error is read before `tree` is dropped (CloseHandle could overwrite it).
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let tree = Self { job };
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let limited = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if limited == 0 || AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            resume(child.id())?;
            Ok(tree)
        }
    }

    #[cfg(unix)]
    fn attach(child: &Child) -> std::io::Result<Self> {
        Ok(Self { pgid: child.id() })
    }

    #[cfg(not(any(windows, unix)))]
    fn attach(_: &Child) -> std::io::Result<Self> {
        Ok(Self {})
    }

    /// Kills the whole tree and reaps the direct child.
    fn kill(&self, child: &mut Child) {
        #[cfg(windows)]
        // SAFETY: `job` is a valid job handle owned by `self`.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1)
        };
        #[cfg(unix)]
        self.kill_group();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    fn kill_group(&self) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.pgid)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // SAFETY: closes the handle created in `attach`. KILL_ON_JOB_CLOSE ends any survivors.
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job)
        };
        // Daemonised leftovers. The group id stays reserved while any member lives, so this
        // cannot hit an unrelated group.
        #[cfg(unix)]
        self.kill_group();
    }
}

/// Resumes the threads of a process created with `CREATE_SUSPENDED` (only its main thread exists).
#[cfg(windows)]
fn resume(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: the snapshot and thread handles are checked, used only here and closed. The entry
    // is zeroed POD with `dwSize` set, as Thread32First requires.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut more = Thread32First(snap, &mut e) != 0;
        while more {
            if e.th32OwnerProcessID == pid {
                let t = OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID);
                if !t.is_null() {
                    resumed += usize::from(ResumeThread(t) != u32::MAX);
                    CloseHandle(t);
                }
            }
            more = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        if resumed == 0 { Err(std::io::Error::other(format!("could not start process {pid}"))) } else { Ok(()) }
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

    /// A shell running a script: PowerShell on Windows, sh elsewhere.
    fn shell(windows: &str, unix: &str) -> Option<(std::path::PathBuf, Vec<String>)> {
        #[cfg(windows)]
        let (prog, args, _) = ("powershell", vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), windows.into()], unix);
        #[cfg(not(windows))]
        let (prog, args, _) = ("sh", vec!["-c".into(), unix.into()], windows);
        Some((crate::tools::which(prog)?, args))
    }

    /// A long-running child is stopped within a poll interval of cancellation.
    #[test]
    fn cancel_kills_running_child() {
        let Some((p, args)) = shell("Start-Sleep 30", "sleep 30") else { return };
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let flag = std::sync::atomic::AtomicBool::new(false);
        let t0 = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                flag.store(true, Ordering::Relaxed);
            });
            let r = run_watched(&p, &args, &flag, &mut |_| {}, None);
            assert!(matches!(r, Err(MediaError::Cancelled)), "{r:?}");
        });
        assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
        assert!(matches!(run_watched(&p, &args, &Cancelled, &mut |_| {}, None), Err(MediaError::Cancelled)));
    }

    /// stdout arrives whole, a tool that only writes to stdout is not idle, and `run_any` keeps
    /// the output of a failing exit.
    #[test]
    fn stdout_counts_as_life() {
        let Some((p, args)) = shell(
            "1..8 | % { [Console]::Out.WriteLine('x' * 1000); [Console]::Out.Flush(); Start-Sleep -Milliseconds 1000 }; exit 3",
            "for i in 1 2 3 4 5 6 7 8; do printf '%01000d\\n' 0; sleep 1; done; exit 3",
        ) else {
            return;
        };
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        // ~8 s of stdout-only output, 1 s apart, against a 6 s idle limit. The limit also covers
        // the shell's start: PowerShell took over 2.5 s to print its first line on the Windows
        // ARM runner with the crate's tests in parallel.
        let out = run_any(&p, &args, &(), Some(Duration::from_secs(6))).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.len() == 1000).count(), 8);
        let (p, args) = shell("exit 3", "exit 3").unwrap();
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        assert!(matches!(run_watched(&p, &args, &(), &mut |_| {}, None), Err(MediaError::Tool { .. })));
    }
}
