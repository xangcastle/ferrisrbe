//! Runs the command of one action as a child process.
//!
//! The child gets exactly the environment and working directory it is
//! given, runs in its own process group, and is bounded by a timeout and a
//! cancellation signal. Whatever ends it, the whole group is killed
//! afterwards, so a background child can neither outlive the action nor
//! hold its stdout open. With [`ProcessSpec::isolate_network`] it runs in
//! network namespace of its own, with loopback only. Its stdout and stderr
//! go to files, of which at most [`ProcessSpec::capture_limit`] bytes each
//! are kept: memory stays flat whatever the action prints.

use std::future::Future;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::warn;

/// `PATH` of a command whose environment does not set one.
pub const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// What to run and under which conditions.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    /// The complete environment; nothing is inherited from the worker.
    pub env: Vec<(String, String)>,
    pub working_dir: PathBuf,
    pub timeout: Duration,
    /// Run without network: only a loopback interface is visible.
    pub isolate_network: bool,
    /// Where stdout and stderr are written; created or replaced.
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    /// Bytes of each stream kept; the rest is read and dropped, so the
    /// process never blocks on a full pipe, and a notice closes the file.
    pub capture_limit: u64,
}

/// One output stream of the process, as kept on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub path: PathBuf,
    /// Bytes in the file, the truncation notice included.
    pub size: u64,
    /// Bytes the process wrote; more than were kept when it was truncated.
    pub written: u64,
}

impl Captured {
    pub fn truncated(&self) -> bool {
        self.written > self.size
    }

    /// The kept bytes; none when the file could not be created.
    pub async fn read(&self) -> std::io::Result<Vec<u8>> {
        match tokio::fs::read(&self.path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            read => read,
        }
    }
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Cancelled,
}

/// The end of a process and everything it wrote.
#[derive(Debug)]
pub struct ProcessOutcome {
    pub termination: Termination,
    pub stdout: Captured,
    pub stderr: Captured,
    pub duration: Duration,
}

impl ProcessOutcome {
    /// The exit code reported to the client: the process's own, `128 + n`
    /// for signal `n` (the shell convention Bazel also uses), and `-1` when
    /// the worker ended it.
    pub fn exit_code(&self) -> i32 {
        match self.termination {
            Termination::Exited(code) => code,
            Termination::Signaled(signal) => 128 + signal,
            Termination::TimedOut | Termination::Cancelled => -1,
        }
    }
}

/// Runs `spec` until it exits, its timeout expires, or `cancel` resolves.
///
/// # Errors
///
/// When the process cannot be spawned (missing program, bad working
/// directory, network isolation unavailable) or waiting on it fails.
pub async fn run(
    spec: ProcessSpec,
    cancel: impl Future<Output = ()>,
) -> std::io::Result<ProcessOutcome> {
    let started = Instant::now();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .current_dir(&spec.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if spec.isolate_network {
        isolate_network(&mut command)?;
    }
    let mut child = command.spawn()?;
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let stdout = tokio::spawn(capture(
        child.stdout.take(),
        spec.stdout_path.clone(),
        spec.capture_limit,
    ));
    let stderr = tokio::spawn(capture(
        child.stderr.take(),
        spec.stderr_path.clone(),
        spec.capture_limit,
    ));

    tokio::pin!(cancel);
    let termination = tokio::select! {
        status = child.wait() => termination_of(status?),
        _ = tokio::time::sleep(spec.timeout) => Termination::TimedOut,
        _ = &mut cancel => Termination::Cancelled,
    };
    kill_group(group);
    let _ = child.wait().await;

    let captured = |task: Result<Captured, _>, path: &PathBuf| {
        task.unwrap_or_else(|_| Captured {
            path: path.clone(),
            size: 0,
            written: 0,
        })
    };
    Ok(ProcessOutcome {
        termination,
        stdout: captured(stdout.await, &spec.stdout_path),
        stderr: captured(stderr.await, &spec.stderr_path),
        duration: started.elapsed(),
    })
}

/// Starts the child in user and network namespaces of its own, with the
/// loopback interface up so it can still serve and connect on localhost.
///
/// A pod without `CAP_SYS_ADMIN` cannot unshare a network namespace alone,
/// but can inside a new user namespace. That namespace maps the worker's
/// uid and gid to themselves, so files keep their owner and permissions.
/// Between fork and exec the hook only makes system calls on buffers
/// allocated before the fork, which is what `pre_exec` requires.
#[cfg(target_os = "linux")]
fn isolate_network(command: &mut Command) -> std::io::Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let uid_map = format!("{uid} {uid} 1");
    let gid_map = format!("{gid} {gid} 1");
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            write_proc_file(c"/proc/self/setgroups", b"deny")?;
            write_proc_file(c"/proc/self/uid_map", uid_map.as_bytes())?;
            write_proc_file(c"/proc/self/gid_map", gid_map.as_bytes())?;
            bring_loopback_up()
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn isolate_network(_: &mut Command) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "network isolation needs Linux namespaces",
    ))
}

#[cfg(target_os = "linux")]
unsafe fn write_proc_file(path: &std::ffi::CStr, content: &[u8]) -> std::io::Result<()> {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let written = libc::write(fd, content.as_ptr().cast(), content.len());
    let result = if written == content.len() as isize {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    };
    libc::close(fd);
    result
}

#[cfg(target_os = "linux")]
unsafe fn bring_loopback_up() -> std::io::Result<()> {
    let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut request: libc::ifreq = std::mem::zeroed();
    for (slot, byte) in request.ifr_name.iter_mut().zip(b"lo") {
        *slot = *byte as libc::c_char;
    }
    let mut result = Ok(());
    if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut request) < 0 {
        result = Err(std::io::Error::last_os_error());
    } else {
        request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &request) < 0 {
            result = Err(std::io::Error::last_os_error());
        }
    }
    libc::close(fd);
    result
}

/// Size of each read from a child's pipe.
const CAPTURE_CHUNK: usize = 64 * 1024;

/// Drains `pipe` into a new file at `path`, keeping its first `limit` bytes
/// and a notice of how many more it dropped. A file that cannot be written
/// stops keeping bytes, but the pipe is still drained to the end.
async fn capture(pipe: Option<impl AsyncRead + Unpin>, path: PathBuf, limit: u64) -> Captured {
    let mut file = match tokio::fs::File::create(&path).await {
        Ok(file) => Some(file),
        Err(e) => {
            warn!("Cannot capture output to {}: {}", path.display(), e);
            None
        }
    };
    let (mut kept, mut written) = (0u64, 0u64);
    if let Some(mut pipe) = pipe {
        let mut buffer = vec![0u8; CAPTURE_CHUNK];
        loop {
            let n = match pipe.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            written += n as u64;
            let room = limit.saturating_sub(kept).min(n as u64) as usize;
            if room == 0 {
                continue;
            }
            if let Some(f) = file.as_mut() {
                if let Err(e) = f.write_all(&buffer[..room]).await {
                    warn!("Stopped capturing output to {}: {}", path.display(), e);
                    file = None;
                    continue;
                }
                kept += room as u64;
            }
        }
    }
    if let Some(f) = file.as_mut() {
        if written > kept {
            let notice = format!("\n[Output truncated: kept {kept} of {written} bytes]\n");
            if f.write_all(notice.as_bytes()).await.is_ok() {
                kept += notice.len() as u64;
            }
        }
        let _ = f.flush().await;
    }
    Captured {
        path,
        size: kept,
        written,
    }
}

fn termination_of(status: ExitStatus) -> Termination {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => Termination::Exited(code),
        (None, Some(signal)) => Termination::Signaled(signal),
        (None, None) => Termination::Exited(-1),
    }
}

/// Kills every process of the group led by the child. `killpg` takes only
/// integers and reports an unknown or already-exited group as `ESRCH`, which
/// is ignored, so the call is sound for any value.
fn kill_group(group: Option<i32>) {
    if let Some(group) = group {
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    fn bash(script: &str, timeout: Duration) -> ProcessSpec {
        ProcessSpec {
            program: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: vec![("PATH".to_string(), DEFAULT_PATH.to_string())],
            working_dir: std::env::temp_dir(),
            timeout,
            isolate_network: false,
            stdout_path: scratch_file("out"),
            stderr_path: scratch_file("err"),
            capture_limit: 1 << 20,
        }
    }

    fn scratch_file(stream: &str) -> PathBuf {
        std::env::temp_dir().join(format!("process-test-{}.{stream}", uuid::Uuid::new_v4()))
    }

    const GENEROUS: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn the_exit_code_and_output_come_back() {
        let outcome = run(bash("echo out; echo err >&2; exit 3", GENEROUS), pending())
            .await
            .unwrap();
        assert_eq!(outcome.termination, Termination::Exited(3));
        assert_eq!(outcome.exit_code(), 3);
        assert_eq!(outcome.stdout.read().await.unwrap(), b"out\n");
        assert_eq!(outcome.stderr.read().await.unwrap(), b"err\n");
    }

    #[tokio::test]
    async fn the_timeout_kills_the_process_and_its_children() {
        let outcome = run(
            bash("sleep 60 & sleep 60", Duration::from_millis(200)),
            pending(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.termination, Termination::TimedOut);
        assert_eq!(outcome.exit_code(), -1);
        assert!(
            outcome.duration < Duration::from_secs(10),
            "{:?}",
            outcome.duration
        );
    }

    #[tokio::test]
    async fn a_background_child_does_not_outlive_the_action() {
        let outcome = run(bash("sleep 60 & echo done", GENEROUS), pending())
            .await
            .unwrap();
        assert_eq!(outcome.termination, Termination::Exited(0));
        assert_eq!(outcome.stdout.read().await.unwrap(), b"done\n");
        assert!(
            outcome.duration < Duration::from_secs(10),
            "{:?}",
            outcome.duration
        );
    }

    #[tokio::test]
    async fn cancellation_kills_the_process() {
        let outcome = run(
            bash("sleep 60", GENEROUS),
            tokio::time::sleep(Duration::from_millis(200)),
        )
        .await
        .unwrap();
        assert_eq!(outcome.termination, Termination::Cancelled);
        assert!(
            outcome.duration < Duration::from_secs(10),
            "{:?}",
            outcome.duration
        );
    }

    #[tokio::test]
    async fn a_signal_is_reported_as_128_plus_its_number() {
        let outcome = run(bash("kill -TERM $$", GENEROUS), pending())
            .await
            .unwrap();
        assert_eq!(outcome.termination, Termination::Signaled(libc::SIGTERM));
        assert_eq!(outcome.exit_code(), 128 + libc::SIGTERM);
    }

    #[tokio::test]
    async fn output_past_the_capture_limit_is_dropped_without_blocking_the_process() {
        let mut spec = bash("head -c 300000 /dev/zero; echo done >&2", GENEROUS);
        spec.capture_limit = 1000;

        let outcome = run(spec, pending()).await.unwrap();

        assert_eq!(outcome.termination, Termination::Exited(0));
        assert!(outcome.stdout.truncated());
        assert_eq!(outcome.stdout.written, 300_000);
        let kept = outcome.stdout.read().await.unwrap();
        assert_eq!(kept.len() as u64, outcome.stdout.size);
        assert!(kept[..1000].iter().all(|b| *b == 0));
        assert_eq!(
            &kept[1000..],
            b"\n[Output truncated: kept 1000 of 300000 bytes]\n"
        );
        assert_eq!(outcome.stderr.read().await.unwrap(), b"done\n");
        assert!(!outcome.stderr.truncated());
    }

    #[tokio::test]
    async fn the_environment_and_working_directory_are_exactly_the_given_ones() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut spec = bash("pwd; env | sort", GENEROUS);
        spec.env.push(("ONLY".to_string(), "this".to_string()));
        spec.working_dir = dir.path().to_path_buf();

        let outcome = run(spec, pending()).await.unwrap();
        let stdout = String::from_utf8(outcome.stdout.read().await.unwrap()).unwrap();
        let mut lines = stdout.lines();
        let pwd = std::fs::canonicalize(lines.next().unwrap()).unwrap();
        assert_eq!(pwd, std::fs::canonicalize(dir.path()).unwrap());
        let env: Vec<&str> = lines
            .filter(|l| !l.starts_with("PWD=") && !l.starts_with("SHLVL=") && !l.starts_with("_="))
            .collect();
        assert_eq!(env, vec!["ONLY=this", &format!("PATH={DEFAULT_PATH}")[..]]);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_isolated_action_sees_only_a_working_loopback() {
        let mut spec = bash(
            "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '; cat /sys/class/net/lo/operstate /sys/class/net/lo/flags",
            GENEROUS,
        );
        spec.isolate_network = true;

        let outcome = run(spec, pending()).await.unwrap();
        let stdout = String::from_utf8(outcome.stdout.read().await.unwrap()).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], "lo", "{stdout}");
        assert_eq!(lines.len(), 3, "{stdout}");
        let flags = i64::from_str_radix(lines[2].trim_start_matches("0x"), 16).unwrap();
        assert_ne!(flags & i64::from(libc::IFF_UP), 0, "{stdout}");
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn network_isolation_is_refused_off_linux() {
        let mut spec = bash("true", GENEROUS);
        spec.isolate_network = true;
        assert!(run(spec, pending()).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let mut spec = bash("true", GENEROUS);
        spec.program = "/does/not/exist".to_string();
        assert!(run(spec, pending()).await.is_err());
    }
}
