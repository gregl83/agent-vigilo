//! Bounded child-process execution and process-tree resource collection.
//!
//! The runner drains output concurrently, caps retained bytes, enforces a hard
//! watchdog, and terminates the complete child tree. Unix process groups and
//! Windows Job Objects provide platform-specific lifecycle ownership.

use std::{
    io::{
        self,
        Read,
    },
    path::{
        Path,
        PathBuf,
    },
    process::{
        Child,
        Command,
        Stdio,
    },
    thread,
    time::{
        Duration,
        Instant,
    },
};

use anyhow::{
    Context,
    Result,
    bail,
};

/// Inputs for one bounded child-process execution.
pub struct ProcessSpec<'a> {
    /// Executable to launch.
    pub program: &'a Path,
    /// Command-line arguments passed without shell interpretation.
    pub args: &'a [String],
    /// Optional working directory for the child.
    pub current_dir: Option<&'a Path>,
    /// Environment variables added or overridden for the child.
    pub env: &'a [(String, String)],
    /// Hard deadline after which the entire child tree is terminated.
    pub timeout: Duration,
    /// Maximum number of stdout bytes retained in memory.
    pub stdout_limit: usize,
    /// Maximum number of stderr bytes retained in memory.
    pub stderr_limit: usize,
}

/// Exit, resource, and captured-output observations for one child process.
#[derive(Debug)]
pub struct ProcessOutcome {
    /// Elapsed wall-clock duration including process cleanup.
    pub wall_time: Duration,
    /// Process-tree CPU time when supported by the collector.
    pub cpu_time_ns: Option<u64>,
    /// Peak process-tree resident memory when supported by the collector.
    pub peak_rss_bytes: Option<u64>,
    /// Stable name of the platform resource collector.
    pub resource_source: &'static str,
    /// Child exit code, or `None` when no code was observable.
    pub exit_code: Option<i32>,
    /// Whether the watchdog deadline caused termination.
    pub timed_out: bool,
    /// Bounded stdout capture.
    pub stdout: CapturedOutput,
    /// Bounded stderr capture.
    pub stderr: CapturedOutput,
}

/// Bounded output retained while separately counting all observed bytes.
#[derive(Debug)]
pub struct CapturedOutput {
    /// Total bytes read from the stream, including discarded overflow.
    pub bytes_seen: u64,
    /// Whether output exceeded the configured retention limit.
    pub truncated: bool,
    /// Retained prefix of the stream.
    pub data: Vec<u8>,
    /// Time from process launch until the first observed byte.
    pub first_byte_time: Option<Duration>,
    /// Time from process launch until the most recently observed byte.
    pub last_byte_time: Option<Duration>,
}

impl CapturedOutput {
    /// Decodes the retained stream prefix with lossy UTF-8 replacement.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// Executes a child under platform process-tree ownership and resource collection.
pub fn execute(spec: &ProcessSpec<'_>) -> Result<ProcessOutcome> {
    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(current_dir) = spec.current_dir {
        command.current_dir(current_dir);
    }
    configure_process_group(&mut command);

    let resource_baseline = capture_resource_baseline()?;
    let started = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", spec.program.display()))?;
    let mut group = ProcessGroup::attach(&mut child, resource_baseline)?;
    let stdout = drain(
        child.stdout.take().context("child stdout was not piped")?,
        spec.stdout_limit,
        started,
    );
    let stderr = drain(
        child.stderr.take().context("child stderr was not piped")?,
        spec.stderr_limit,
        started,
    );

    let (status, timed_out) = loop {
        group.observe(child.id());
        if let Some(status) = child.try_wait().context("poll child")? {
            break (status, false);
        }
        if started.elapsed() >= spec.timeout {
            group.terminate(&mut child)?;
            let status = child.wait().context("reap timed-out child")?;
            break (status, true);
        }
        thread::sleep(Duration::from_millis(2));
    };
    let resources = group.resources();
    let stdout = stdout
        .join()
        .map_err(|_| anyhow::anyhow!("stdout drain thread panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| anyhow::anyhow!("stderr drain thread panicked"))??;

    Ok(ProcessOutcome {
        wall_time: started.elapsed(),
        cpu_time_ns: resources.cpu_time_ns,
        peak_rss_bytes: resources.peak_rss_bytes,
        resource_source: resources.source,
        exit_code: status.code(),
        timed_out,
        stdout,
        stderr,
    })
}

/// Drains one output stream without blocking the child and retains at most `limit` bytes.
fn drain<R: Read + Send + 'static>(
    mut reader: R,
    limit: usize,
    started: Instant,
) -> thread::JoinHandle<io::Result<CapturedOutput>> {
    thread::spawn(move || {
        let mut data = Vec::with_capacity(limit.min(8192));
        let mut bytes_seen = 0_u64;
        let mut first_byte_time = None;
        let mut last_byte_time = None;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let observed_at = started.elapsed();
            first_byte_time.get_or_insert(observed_at);
            last_byte_time = Some(observed_at);
            bytes_seen = bytes_seen.saturating_add(read as u64);
            let remaining = limit.saturating_sub(data.len());
            data.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        Ok(CapturedOutput {
            bytes_seen,
            truncated: bytes_seen > data.len() as u64,
            data,
            first_byte_time,
            last_byte_time,
        })
    })
}

struct Resources {
    cpu_time_ns: Option<u64>,
    peak_rss_bytes: Option<u64>,
    source: &'static str,
}

#[cfg(unix)]
type ResourceBaseline = libc::rusage;

#[cfg(unix)]
/// Captures cumulative child usage before launch so one execution can be isolated.
fn capture_resource_baseline() -> Result<ResourceBaseline> {
    let mut usage = std::mem::MaybeUninit::uninit();
    let result = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr()) };
    if result != 0 {
        bail!(
            "capture child resource baseline: {}",
            io::Error::last_os_error()
        );
    }
    Ok(unsafe { usage.assume_init() })
}

#[cfg(not(unix))]
struct ResourceBaseline;

#[cfg(not(unix))]
/// Uses an empty baseline on platforms where the controller reports direct usage.
fn capture_resource_baseline() -> Result<ResourceBaseline> {
    Ok(ResourceBaseline)
}

#[cfg(unix)]
/// Places the Unix child in a new process group for whole-tree termination.
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
/// Defers process-tree ownership to the Windows Job Object controller.
fn configure_process_group(_command: &mut Command) {}

#[cfg(not(any(unix, windows)))]
/// Leaves process grouping unchanged on unsupported platforms.
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
struct ProcessGroup {
    process_group_id: i32,
    usage_before: ResourceBaseline,
    peak_rss_bytes: u64,
}

#[cfg(unix)]
impl ProcessGroup {
    fn attach(child: &mut Child, usage_before: ResourceBaseline) -> Result<Self> {
        Ok(Self {
            process_group_id: child.id() as i32,
            usage_before,
            peak_rss_bytes: 0,
        })
    }

    fn observe(&mut self, process_id: u32) {
        #[cfg(target_os = "linux")]
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{process_id}/status")) {
            let peak_kib = status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            });
            if let Some(peak_kib) = peak_kib {
                self.peak_rss_bytes = self.peak_rss_bytes.max(peak_kib.saturating_mul(1024));
            }
        }
    }

    fn terminate(&self, child: &mut Child) -> Result<()> {
        // The child is the process-group leader, so a negative PID reaches descendants too.
        let result = unsafe { libc::kill(-self.process_group_id, libc::SIGKILL) };
        if result != 0 {
            child.kill().context("kill timed-out child")?;
        }
        Ok(())
    }

    fn resources(&self) -> Resources {
        let mut usage_after = std::mem::MaybeUninit::uninit();
        let usage_after =
            if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage_after.as_mut_ptr()) } == 0 {
                Some(unsafe { usage_after.assume_init() })
            } else {
                None
            };
        let cpu_time_ns = usage_after.map(|usage_after| {
            let before = timeval_ns(self.usage_before.ru_utime)
                .saturating_add(timeval_ns(self.usage_before.ru_stime));
            let after =
                timeval_ns(usage_after.ru_utime).saturating_add(timeval_ns(usage_after.ru_stime));
            after.saturating_sub(before).max(0) as u64
        });
        Resources {
            cpu_time_ns,
            peak_rss_bytes: (self.peak_rss_bytes > 0).then_some(self.peak_rss_bytes),
            source: if cfg!(target_os = "linux") {
                "linux-proc-rusage-v1"
            } else {
                "unix-rusage-v1"
            },
        }
    }
}

#[cfg(unix)]
fn timeval_ns(value: libc::timeval) -> i128 {
    i128::from(value.tv_sec)
        .saturating_mul(1_000_000_000)
        .saturating_add(i128::from(value.tv_usec).saturating_mul(1_000))
}

#[cfg(windows)]
struct ProcessGroup {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ProcessGroup {
    fn attach(child: &mut Child, _baseline: ResourceBaseline) -> Result<Self> {
        use std::{
            ffi::c_void,
            mem::size_of,
            os::windows::io::AsRawHandle,
            ptr,
        };

        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject,
            CreateJobObjectW,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            let _ = child.kill();
            bail!("create Windows job object: {}", io::Error::last_os_error());
        }
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const information).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        let assigned = configured != 0
            && unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } != 0;
        if !assigned {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "assign child to Windows job object: {}",
                io::Error::last_os_error()
            );
        }
        Ok(Self { handle })
    }

    fn observe(&mut self, _process_id: u32) {}

    fn terminate(&self, child: &mut Child) -> Result<()> {
        let terminated =
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1) };
        if terminated == 0 {
            child.kill().context("kill timed-out child")?;
        }
        Ok(())
    }

    fn resources(&self) -> Resources {
        use std::{
            ffi::c_void,
            mem::size_of,
            ptr,
        };

        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };

        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let accounting_ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast::<c_void>(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        } != 0;
        let mut extended = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let extended_ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                (&raw mut extended).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        } != 0;
        Resources {
            cpu_time_ns: accounting_ok.then(|| {
                (accounting
                    .TotalUserTime
                    .saturating_add(accounting.TotalKernelTime))
                .max(0) as u64
                    * 100
            }),
            peak_rss_bytes: extended_ok.then_some(extended.PeakJobMemoryUsed as u64),
            source: "windows-job-object-v1",
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessGroup;

#[cfg(not(any(unix, windows)))]
impl ProcessGroup {
    fn attach(_child: &mut Child, _baseline: ResourceBaseline) -> Result<Self> {
        Ok(Self)
    }

    fn observe(&mut self, _process_id: u32) {}

    fn terminate(&self, child: &mut Child) -> Result<()> {
        child.kill().context("kill timed-out child")
    }

    fn resources(&self) -> Resources {
        Resources {
            cpu_time_ns: None,
            peak_rss_bytes: None,
            source: "unavailable",
        }
    }
}

/// Resolves and validates a path to a regular executable file.
pub fn executable_path(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("executable does not exist: {}", path.display());
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const FIXTURE_TEST: &str = "perf::process::tests::subprocess_fixture";

    fn fixture_command() -> (PathBuf, Vec<String>) {
        (
            std::env::current_exe().unwrap(),
            vec!["--exact".into(), FIXTURE_TEST.into(), "--nocapture".into()],
        )
    }

    #[test]
    fn subprocess_fixture() {
        let delay_ms = std::env::var("VIGILO_PERF_TEST_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let output_bytes = std::env::var("VIGILO_PERF_TEST_OUTPUT_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        std::thread::sleep(Duration::from_millis(delay_ms));
        if output_bytes == 0 {
            println!("Usage: Commands:");
        } else {
            std::io::stdout()
                .write_all(&vec![b'x'; output_bytes])
                .unwrap();
            std::io::stdout().flush().unwrap();
        }
    }

    #[test]
    fn output_is_drained_and_bounded() {
        let (program, args) = fixture_command();
        let outcome = execute(&ProcessSpec {
            program: &program,
            args: &args,
            current_dir: None,
            env: &[("VIGILO_PERF_TEST_OUTPUT_BYTES".into(), "4096".into())],
            timeout: Duration::from_secs(30),
            stdout_limit: 64,
            stderr_limit: 64,
        })
        .unwrap();
        assert_eq!(outcome.stdout.data.len(), 64);
        assert!(outcome.stdout.bytes_seen >= 4096);
        assert!(outcome.stdout.truncated);
        assert!(outcome.stdout.first_byte_time.is_some());
        assert!(outcome.stdout.last_byte_time >= outcome.stdout.first_byte_time);
        assert!(outcome.stdout.last_byte_time <= Some(outcome.wall_time));
    }

    #[test]
    fn timeout_kills_and_reaps_child() {
        let (program, args) = fixture_command();
        let outcome = execute(&ProcessSpec {
            program: &program,
            args: &args,
            current_dir: None,
            env: &[("VIGILO_PERF_TEST_DELAY_MS".into(), "10000".into())],
            timeout: Duration::from_millis(30),
            stdout_limit: 64,
            stderr_limit: 64,
        })
        .unwrap();
        assert!(outcome.timed_out);
        assert!(outcome.wall_time < Duration::from_secs(2));
    }
}
