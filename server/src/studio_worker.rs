//! The Head & Tail Studio's crash boundary: every upload is processed by a short-lived
//! child process, `arena studio-worker`, so nothing an upload does can take the server
//! (and the games it runs) down with it.
//!
//! [`crate::design_kit`] bounds its inputs, but its SVG path depends on a prescan that
//! mirrors usvg exactly; a reference loop the prescan misses recurses until the stack
//! overflows, which aborts the process no matter how big the stack is. In a child, an
//! abort, a runaway allocation or a CPU spin costs one upload instead.
//!
//! # Protocol
//!
//! The server runs `<current exe> studio-worker [--fix=flip] [--fix=fit]` with an empty
//! environment, writes the upload to its stdin and closes it. The worker runs
//! [`design_kit::process_on_big_stack`] (a 64 MiB stack) and writes one JSON [`Reply`]
//! to stdout. Exit codes ([`exit`]):
//!
//! | Code | Meaning | Server response |
//! |---|---|---|
//! | 0 | a `shape` or an `error` reply (the upload's fault) | 200, or 422 with the error |
//! | 70 | an `internal` reply: a caught panic, a thread that couldn't start | 500 |
//! | 64 | bad arguments | 500 |
//! | 74 | stdin or stdout failed | 500 |
//! | 101 | a panic outside the processing thread | 500 |
//! | killed by a signal | a crash: stack overflow, failed allocation at the data limit (abort), CPU limit (SIGXCPU), the OOM killer | 422 `too_complex` |
//! | killed at the deadline | no answer in time | 503 `busy` |
//!
//! # Limits
//!
//! Set in the child between fork and exec ([`WorkerLimits`]): `RLIMIT_CPU` (SIGXCPU at
//! the soft limit, SIGKILL a second later), `RLIMIT_DATA` (allocations past it fail and
//! the worker aborts), `RLIMIT_CORE` 0 (a crash never writes a core file), and
//! `oom_score_adj` 1000 on Linux, so when memory runs out the kernel kills the worker
//! rather than the server. The parent kills the worker at the wall-clock deadline and
//! drops it with `kill_on_drop`, so a cancelled request stops the work too.
//!
//! The worker must not start anything the server does: `main.rs` dispatches to
//! [`worker_main`] before Sentry, config, telemetry or the database.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use color_eyre::eyre::eyre;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};

use crate::design_kit::{
    self, CleanShape, FillRule, Fix, InputFormat, Limits, Lint, Metrics, ProcessError, Severity,
    Strategy,
};

/// The hidden subcommand: `arena studio-worker`.
pub const SUBCOMMAND: &str = "studio-worker";

/// Most upload bytes the worker reads: the largest per-format cap. Anything longer is
/// read up to one byte past this, which `process_upload` rejects as `too_large`.
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Most stdout the server accepts: a 64 KiB path plus metrics and lint copy fits many
/// times over.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Most stderr kept for the log when a worker dies.
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// Exit codes of `arena studio-worker` (sysexits-style).
pub mod exit {
    /// A `shape` or `error` reply is on stdout.
    pub const REPLIED: i32 = 0;
    /// Bad arguments (EX_USAGE).
    pub const USAGE: i32 = 64;
    /// An `internal` reply is on stdout: a bug (EX_SOFTWARE).
    pub const INTERNAL: i32 = 70;
    /// Reading stdin or writing stdout failed (EX_IOERR).
    pub const IO: i32 = 74;
    /// A `--self-test` that a resource limit should have stopped reached its ceiling
    /// instead (EX_TEMPFAIL). Only the self-tests use it.
    pub const SELF_TEST_SURVIVED: i32 = 75;
}

// ---- the JSON the worker writes and the studio endpoint returns ---------------------

/// One processed upload: the studio endpoint's 200 body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapeJson {
    /// The downloadable SVG: exactly [`CleanShape::to_svg`].
    pub svg: String,
    /// Absolute `M`/`L`/`Q`/`C`/`Z` commands; matches `^[MLQCZ0-9 .\-]*$`.
    pub path_d: String,
    pub fill_rule: FillRule,
    pub strategy: Strategy,
    pub input: InputFormat,
    pub metrics: Metrics,
    /// Shape lints for each kind; the page switches kinds without re-posting.
    pub lints: KindLintsJson,
    /// Kind-independent input facts (tips and info).
    pub info: Vec<LintJson>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindLintsJson {
    pub head: Vec<LintJson>,
    pub tail: Vec<LintJson>,
}

/// One lint as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LintJson {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    /// The one-tap fix to offer beside it.
    pub fix: Option<Fix>,
    /// Anchor in the studio guide (`#neck`, `#direction`, ...).
    pub guide: String,
}

/// A user-facing error: `{"error": {"code", "message"}}` in a 4xx/503 body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorJson {
    /// Stable snake_case identifier ([`ProcessError::code`], or `busy`, `rate_limited`,
    /// `too_large`, ...).
    pub code: String,
    pub message: String,
}

impl ErrorJson {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    /// What a crashed worker means for the artist: the file was too much to process.
    pub fn too_complex() -> Self {
        Self::from(&ProcessError::TooComplex("the worker died"))
    }
}

impl From<&ProcessError> for ErrorJson {
    fn from(e: &ProcessError) -> Self {
        Self::new(e.code(), e.user_message())
    }
}

impl From<&Lint> for LintJson {
    fn from(l: &Lint) -> Self {
        Self {
            code: l.code().to_string(),
            severity: l.severity(),
            message: l.message(),
            fix: l.fix(),
            guide: l.guide_anchor().to_string(),
        }
    }
}

impl From<&CleanShape> for ShapeJson {
    fn from(s: &CleanShape) -> Self {
        let lints = |ls: &[Lint]| ls.iter().map(LintJson::from).collect();
        Self {
            svg: s.to_svg(),
            path_d: s.path_d().to_string(),
            fill_rule: s.fill_rule(),
            strategy: s.strategy(),
            input: s.input(),
            metrics: s.metrics().clone(),
            lints: KindLintsJson {
                head: lints(&s.lints().head),
                tail: lints(&s.lints().tail),
            },
            info: lints(s.info()),
        }
    }
}

/// `d` uses only the characters the design kit emits.
pub fn is_clean_path_d(d: &str) -> bool {
    d.bytes().all(|b| {
        matches!(
            b,
            b'M' | b'L' | b'Q' | b'C' | b'Z' | b'0'..=b'9' | b' ' | b'.' | b'-'
        )
    })
}

impl ShapeJson {
    /// Re-check what a worker sent before serving it: the path is clean and within the
    /// size limit, and the SVG is exactly the fixed template around it.
    fn check(&self) -> Result<(), &'static str> {
        if self.path_d.len() > Limits::default().max_path_d_bytes {
            return Err("path_d is over the size limit");
        }
        if !is_clean_path_d(&self.path_d) {
            return Err("path_d has characters outside the path alphabet");
        }
        // The same template as `CleanShape::to_svg` (a test keeps them equal).
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><path fill-rule=\"{}\" d=\"{}\"/></svg>",
            self.fill_rule.as_svg(),
            self.path_d
        );
        if self.svg != svg {
            return Err("svg is not the clean-path template");
        }
        Ok(())
    }
}

/// What the worker writes to stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Shape(ShapeJson),
    /// Caused by the upload: shown to the artist.
    Error(ErrorJson),
    /// A bug (exit code [`exit::INTERNAL`]).
    Internal {
        message: String,
    },
}

// ---- the result the studio endpoint maps to a response ------------------------------

/// How processing one upload ended, whether in the worker or in this process.
#[derive(Debug)]
pub enum Outcome {
    /// 200.
    Shape(Box<ShapeJson>),
    /// The upload's fault: 422 with this error.
    Rejected(ErrorJson),
    /// The worker died (a crash, a resource limit, the OOM killer): 422 `too_complex`,
    /// logged at warn.
    Crashed {
        status: ExitStatus,
        /// The start of the worker's stderr (a panic or abort message), for the log.
        stderr: String,
    },
    /// No answer before the deadline; the work was stopped: 503 `busy`, logged at warn.
    TimedOut,
    /// A bug: 500.
    Internal(color_eyre::Report),
}

impl Outcome {
    /// Map an in-process [`design_kit::process_upload`] result.
    pub fn from_result(result: Result<CleanShape, ProcessError>) -> Self {
        match result {
            Ok(shape) => Outcome::Shape(Box::new(ShapeJson::from(&shape))),
            Err(e) if e.is_internal() => {
                Outcome::Internal(color_eyre::Report::new(e).wrap_err("design kit internal error"))
            }
            Err(e) => Outcome::Rejected(ErrorJson::from(&e)),
        }
    }
}

// ---- the child: `arena studio-worker` ------------------------------------------------

/// Entry point of `arena studio-worker`; `args` are the arguments after the subcommand.
/// Returns the process exit code (see [`exit`]).
pub fn worker_main(args: impl IntoIterator<Item = String>) -> i32 {
    let mut fixes = Vec::new();
    for arg in args {
        if let Some(fix) = arg.strip_prefix("--fix=").and_then(Fix::parse) {
            fixes.push(fix);
        } else if let Some(mode) = arg.strip_prefix("--self-test=") {
            return self_test(mode);
        } else {
            eprintln!("{SUBCOMMAND}: unexpected argument {arg:?}");
            return exit::USAGE;
        }
    }

    let mut bytes = Vec::new();
    let read = std::io::stdin()
        .lock()
        .take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    if let Err(e) = read {
        eprintln!("{SUBCOMMAND}: reading stdin: {e}");
        return exit::IO;
    }

    let result = design_kit::process_on_big_stack(bytes, &Limits::default(), &fixes, ())
        .blocking_recv()
        .unwrap_or(Err(ProcessError::Internal(
            "the processing thread ended without a result",
        )));
    let (reply, code) = match result {
        Ok(shape) => (Reply::Shape(ShapeJson::from(&shape)), exit::REPLIED),
        Err(e) if e.is_internal() => (
            Reply::Internal {
                message: e.to_string(),
            },
            exit::INTERNAL,
        ),
        Err(e) => (Reply::Error(ErrorJson::from(&e)), exit::REPLIED),
    };

    let mut out = std::io::stdout().lock();
    let written = serde_json::to_writer(&mut out, &reply)
        .map_err(std::io::Error::from)
        .and_then(|()| out.flush());
    if let Err(e) = written {
        eprintln!("{SUBCOMMAND}: writing stdout: {e}");
        return exit::IO;
    }
    code
}

/// Test-only misbehaviour, to prove the limits and the parent's handling work against
/// the real binary (`server/tests/studio_worker.rs`). The server never passes
/// `--self-test`: it only ever adds `--fix=` arguments. Each mode has a ceiling so that
/// a limit that fails to apply fails the test instead of eating the machine.
fn self_test(mode: &str) -> i32 {
    let ceiling = std::time::Instant::now() + Duration::from_secs(30);
    match mode {
        // What a stack overflow or a failed allocation looks like from outside.
        "abort" => std::process::abort(),
        // A memory bomb: allocate and touch 16 MiB at a time, up to 2 GiB.
        "alloc" => {
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..128 {
                held.push(vec![1; 16 << 20]);
            }
            eprintln!("{SUBCOMMAND}: allocated {} MiB", held.len() * 16);
            exit::SELF_TEST_SURVIVED
        }
        // A CPU hog.
        "spin" => {
            let mut x: u64 = 1;
            while std::time::Instant::now() < ceiling {
                for _ in 0..1_000_000 {
                    x = std::hint::black_box(
                        x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1),
                    );
                }
            }
            exit::SELF_TEST_SURVIVED
        }
        // A worker that never answers (and uses no CPU).
        "hang" => {
            std::thread::sleep(ceiling.saturating_duration_since(std::time::Instant::now()));
            exit::SELF_TEST_SURVIVED
        }
        _ => {
            eprintln!("{SUBCOMMAND}: unknown self-test {mode:?}");
            exit::USAGE
        }
    }
}

// ---- the parent: spawning a worker ---------------------------------------------------

/// Resource limits for one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLimits {
    /// `RLIMIT_CPU` soft limit, in seconds of CPU time: SIGXCPU kills the worker (the
    /// hard limit, a second later, is SIGKILL).
    pub cpu_secs: u64,
    /// `RLIMIT_DATA` in bytes: the worker's private writable memory (heap and thread
    /// stacks, including the 64 MiB processing stack, which counts in full although
    /// only the touched pages use memory). Past it, allocations fail and the worker
    /// aborts.
    pub data_bytes: u64,
    /// Wall-clock deadline: the worker is killed after this long. Waiting this long
    /// means the machine is busy, not that the upload is too big (the CPU limit catches
    /// that first).
    pub wall: Duration,
}

/// Default `RLIMIT_DATA` for a worker: 192 MiB.
///
/// Measured with `prlimit --data` (`docs/design-kit.md`, "The worker process"): the
/// heaviest legitimate uploads (2048 px PNG and JPEG, a 1448 px progressive JPEG, 16
/// nested clips retraced) need 88 MiB, the 64 MiB processing stack included, and fail
/// at 80 MiB. 192 MiB leaves twice that, and a memory bomb can touch at most ~128 MiB
/// of heap: within what a 512 MiB Cloud Run instance has free beside the server.
pub const DEFAULT_DATA_LIMIT_BYTES: u64 = 192 << 20;

impl Default for WorkerLimits {
    /// Release: 5 s of CPU (the slowest accepted uploads take 0.25 s) and 10 s of wall
    /// clock. Debug builds (local dev and the e2e server) run the pipeline about 30x
    /// slower (a 1448 px progressive JPEG takes 7 s), so they get 30 s and 45 s.
    fn default() -> Self {
        let debug = cfg!(debug_assertions);
        Self {
            cpu_secs: if debug { 30 } else { 5 },
            data_bytes: DEFAULT_DATA_LIMIT_BYTES,
            wall: Duration::from_secs(if debug { 45 } else { 10 }),
        }
    }
}

/// How to start `arena studio-worker`: the binary and its limits.
#[derive(Debug, Clone)]
pub struct Worker {
    program: PathBuf,
    limits: WorkerLimits,
    self_test: Option<String>,
}

impl Worker {
    /// Workers run `program` (normally the server's own executable).
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            limits: WorkerLimits::default(),
            self_test: None,
        }
    }

    /// Workers run this process's own executable.
    ///
    /// On Linux that is `/proc/self/exe`, which (in the forked child, as in the parent)
    /// names the running server's executable even after the file on disk was replaced
    /// or deleted by a deploy or a rebuild, so a worker is always the same build as the
    /// server that started it. Elsewhere, the path the server started from.
    pub fn current_exe() -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        if std::path::Path::new("/proc/self/exe").exists() {
            return Ok(Self::new("/proc/self/exe"));
        }
        Ok(Self::new(std::env::current_exe()?))
    }

    pub fn with_limits(mut self, limits: WorkerLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> &WorkerLimits {
        &self.limits
    }

    /// Tests only: make the worker misbehave in one of the ways the limits must stop
    /// (`abort`, `alloc`, `spin`, `hang`).
    #[doc(hidden)]
    pub fn with_self_test(mut self, mode: &str) -> Self {
        self.self_test = Some(mode.to_string());
        self
    }

    /// Process one upload in a new worker and wait for it to finish (or kill it at the
    /// deadline). When this returns, the worker has exited and been reaped; if the
    /// future is dropped first, the worker is killed.
    pub async fn run(&self, bytes: &[u8], fixes: &[Fix]) -> Outcome {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.arg(SUBCOMMAND)
            .args(fixes.iter().map(|f| format!("--fix={}", f.as_str())))
            .args(self.self_test.iter().map(|m| format!("--self-test={m}")))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        apply_limits(&mut cmd, &self.limits);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Outcome::Internal(
                    color_eyre::Report::new(e).wrap_err("could not start the studio worker"),
                );
            }
        };
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());

        // Write stdin while reading stdout and stderr, so neither side blocks on a full
        // pipe; then reap the worker.
        let talk = async {
            let write = async {
                if let Some(mut stdin) = stdin {
                    // A worker that exits early (a crash, a self-test) closes its end:
                    // the broken pipe is expected and its exit status says why.
                    let _ = stdin.write_all(bytes).await;
                    let _ = stdin.shutdown().await;
                }
            };
            let ((), out, err) = tokio::join!(
                write,
                read_capped(stdout, MAX_REPLY_BYTES),
                read_capped(stderr, MAX_STDERR_BYTES)
            );
            (out, err, child.wait().await)
        };
        let finished = tokio::time::timeout(self.limits.wall, talk).await;
        match finished {
            Ok(((stdout, overflow), (stderr, _), Ok(status))) => {
                classify(status, &stdout, overflow, &stderr)
            }
            Ok((_, _, Err(e))) => Outcome::Internal(
                color_eyre::Report::new(e).wrap_err("waiting for the studio worker failed"),
            ),
            Err(_elapsed) => {
                // Stop the CPU work, and reap the worker so it doesn't linger as a
                // zombie. `kill` sends SIGKILL and waits.
                if let Err(e) = child.kill().await {
                    tracing::warn!(error = %e, "could not kill a timed-out studio worker");
                }
                Outcome::TimedOut
            }
        }
    }
}

/// Read up to `cap` bytes, then drain (and drop) the rest so the worker never blocks on
/// a full pipe. Returns what was kept and whether anything was dropped.
async fn read_capped(reader: Option<impl AsyncRead + Unpin>, cap: usize) -> (Vec<u8>, bool) {
    let Some(mut reader) = reader else {
        return (Vec::new(), false);
    };
    let mut kept = Vec::new();
    // A read error just ends the output early; the exit status tells the story.
    let _ = (&mut reader).take(cap as u64).read_to_end(&mut kept).await;
    let dropped = tokio::io::copy(&mut reader, &mut tokio::io::sink())
        .await
        .unwrap_or(0);
    (kept, dropped > 0)
}

/// Turn a finished worker's exit status and output into an [`Outcome`].
fn classify(status: ExitStatus, stdout: &[u8], overflow: bool, stderr: &[u8]) -> Outcome {
    let stderr_text = || {
        let text = String::from_utf8_lossy(stderr);
        let text = text.trim();
        let end = text.char_indices().nth(500).map_or(text.len(), |(i, _)| i);
        text[..end].to_string()
    };
    match status.code() {
        // Killed by a signal: a crash or a resource limit.
        None => Outcome::Crashed {
            status,
            stderr: stderr_text(),
        },
        Some(exit::REPLIED | exit::INTERNAL) => {
            if overflow {
                return Outcome::Internal(eyre!("the studio worker's reply was too long"));
            }
            let reply: Reply = match serde_json::from_slice(stdout) {
                Ok(reply) => reply,
                Err(e) => {
                    return Outcome::Internal(
                        color_eyre::Report::new(e)
                            .wrap_err(format!("unreadable studio worker reply ({status})")),
                    );
                }
            };
            match (status.code(), reply) {
                (Some(exit::REPLIED), Reply::Shape(shape)) => match shape.check() {
                    Ok(()) => Outcome::Shape(Box::new(shape)),
                    Err(why) => Outcome::Internal(eyre!("bad studio worker reply: {why}")),
                },
                (Some(exit::REPLIED), Reply::Error(error)) => Outcome::Rejected(error),
                (Some(exit::INTERNAL), Reply::Internal { message }) => {
                    Outcome::Internal(eyre!("studio worker internal error: {message}"))
                }
                (code, reply) => Outcome::Internal(eyre!(
                    "studio worker exit code {code:?} doesn't match its reply {reply:?}"
                )),
            }
        }
        Some(code) => Outcome::Internal(eyre!(
            "studio worker exited with code {code}: {}",
            stderr_text()
        )),
    }
}

/// Set the worker's resource limits between fork and exec.
#[cfg(unix)]
fn apply_limits(cmd: &mut tokio::process::Command, limits: &WorkerLimits) {
    let cpu: libc::rlim_t = limits.cpu_secs;
    let data: libc::rlim_t = limits.data_bytes;
    // SAFETY: the closure runs in the forked child before exec, where only
    // async-signal-safe functions may be called. It calls getrlimit, setrlimit, open,
    // write and close, captures two integers and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            lower_limit(libc::RLIMIT_CORE, 0, 0)?;
            lower_limit(libc::RLIMIT_CPU, cpu, cpu.saturating_add(1))?;
            lower_limit(libc::RLIMIT_DATA, data, data)?;
            #[cfg(target_os = "linux")]
            prefer_for_oom_kill();
            Ok(())
        });
    }
}

/// The `resource` argument's type differs between libcs.
#[cfg(all(unix, target_os = "linux", target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
type Resource = libc::c_int;

/// Lower a limit to `soft`/`hard`, never above what the process already has (raising a
/// hard limit needs privileges).
#[cfg(unix)]
fn lower_limit(resource: Resource, soft: libc::rlim_t, hard: libc::rlim_t) -> std::io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `current` is a valid, writable rlimit.
    if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let max = hard.min(current.rlim_max);
    let new = libc::rlimit {
        rlim_cur: soft.min(max),
        rlim_max: max,
    };
    // SAFETY: `new` is a valid rlimit.
    if unsafe { libc::setrlimit(resource, &new) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `oom_score_adj = 1000`: when memory runs out, the kernel kills the worker first.
/// Raising it needs no privileges. Best effort: without `/proc` the limits still apply.
#[cfg(target_os = "linux")]
fn prefer_for_oom_kill() {
    const VALUE: &[u8] = b"1000";
    // SAFETY: a NUL-terminated path, a valid buffer, and a descriptor we close.
    unsafe {
        let fd = libc::open(
            c"/proc/self/oom_score_adj".as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        );
        if fd >= 0 {
            libc::write(fd, VALUE.as_ptr().cast(), VALUE.len());
            libc::close(fd);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;

    fn shape() -> ShapeJson {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 0H10V10H0Z"/></svg>"#;
        let clean = design_kit::process_upload(svg, &Limits::default(), &[]).unwrap();
        ShapeJson::from(&clean)
    }

    fn reply_bytes(reply: &Reply) -> Vec<u8> {
        serde_json::to_vec(reply).unwrap()
    }

    #[test]
    fn shape_json_matches_the_clean_shape_and_passes_its_own_check() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 0H10V10H0Z"/></svg>"#;
        let clean = design_kit::process_upload(svg, &Limits::default(), &[]).unwrap();
        let json = ShapeJson::from(&clean);
        assert_eq!(json.svg, clean.to_svg());
        assert_eq!(json.path_d, clean.path_d());
        assert_eq!(json.check(), Ok(()));
        let value = serde_json::to_value(&json).unwrap();
        assert_eq!(value["fill_rule"], clean.fill_rule().as_svg());
        assert_eq!(value["strategy"], clean.strategy().as_str());
        assert_eq!(value["input"], "svg");
        assert!(value["metrics"]["fill_pct"].is_number());
        assert!(value["lints"]["head"].is_array());
    }

    #[test]
    fn lint_json_carries_code_severity_fix_and_anchor() {
        let lint = Lint::FacesLeft { centroid_x: 60.0 };
        let json = serde_json::to_value(LintJson::from(&lint)).unwrap();
        assert_eq!(json["code"], "faces_left");
        assert_eq!(json["severity"], "warn");
        assert_eq!(json["fix"], "flip");
        assert_eq!(json["guide"], "#direction");
        assert_eq!(json["message"], lint.message());
        let none = serde_json::to_value(LintJson::from(&Lint::GuidesVisible)).unwrap();
        assert_eq!(none["fix"], serde_json::Value::Null);
        assert_eq!(none["severity"], "info");
    }

    #[test]
    fn classify_accepts_only_well_formed_replies() {
        let ok = ExitStatus::from_raw(0);
        let shape = shape();
        assert!(matches!(
            classify(ok, &reply_bytes(&Reply::Shape(shape.clone())), false, b""),
            Outcome::Shape(s) if *s == shape
        ));
        let error = ErrorJson::new("unsupported_format", "nope");
        assert!(matches!(
            classify(ok, &reply_bytes(&Reply::Error(error.clone())), false, b""),
            Outcome::Rejected(e) if e == error
        ));

        // A tampered or truncated reply is a bug, never served.
        for d in ["M0 0L1 1Z\"/><script>", "M0 0 x"] {
            let mut bad = shape.clone();
            bad.path_d = d.to_string();
            assert!(matches!(
                classify(ok, &reply_bytes(&Reply::Shape(bad)), false, b""),
                Outcome::Internal(_)
            ));
        }
        let mut bad = shape.clone();
        bad.svg = format!("<svg onload=x>{}", bad.svg);
        assert!(matches!(
            classify(ok, &reply_bytes(&Reply::Shape(bad)), false, b""),
            Outcome::Internal(_)
        ));
        assert!(matches!(
            classify(ok, b"{\"shape\":", false, b""),
            Outcome::Internal(_)
        ));
        assert!(matches!(
            classify(ok, &reply_bytes(&Reply::Shape(shape.clone())), true, b""),
            Outcome::Internal(_)
        ));
        // The exit code and the reply must agree.
        let internal = Reply::Internal {
            message: "boom".into(),
        };
        assert!(matches!(
            classify(
                ExitStatus::from_raw(exit::INTERNAL << 8),
                &reply_bytes(&internal),
                false,
                b""
            ),
            Outcome::Internal(_)
        ));
        assert!(matches!(
            classify(ok, &reply_bytes(&internal), false, b""),
            Outcome::Internal(_)
        ));
    }

    #[test]
    fn classify_maps_exit_codes_and_signals() {
        // Signals: crashes and resource limits.
        for signal in [libc::SIGABRT, libc::SIGKILL, libc::SIGSEGV, libc::SIGXCPU] {
            let out = classify(
                ExitStatus::from_raw(signal),
                b"",
                false,
                b"memory allocation of 16777216 bytes failed\n",
            );
            match out {
                Outcome::Crashed { status, stderr } => {
                    assert_eq!(status.signal(), Some(signal));
                    assert_eq!(stderr, "memory allocation of 16777216 bytes failed");
                }
                other => panic!("signal {signal}: {other:?}"),
            }
        }
        // Usage, I/O, a stray panic, anything else: bugs.
        for code in [exit::USAGE, exit::IO, 101, 1] {
            assert!(
                matches!(
                    classify(ExitStatus::from_raw(code << 8), b"", false, b""),
                    Outcome::Internal(_)
                ),
                "exit {code}"
            );
        }
    }

    #[test]
    fn classify_keeps_only_the_start_of_stderr() {
        let long = "é".repeat(2000);
        let Outcome::Crashed { stderr, .. } = classify(
            ExitStatus::from_raw(libc::SIGABRT),
            b"",
            false,
            long.as_bytes(),
        ) else {
            panic!("a signal is a crash");
        };
        assert_eq!(stderr.chars().count(), 500);
    }

    #[test]
    fn outcome_from_result_separates_user_errors_from_bugs() {
        assert!(matches!(
            Outcome::from_result(Err(ProcessError::EmptyFile)),
            Outcome::Rejected(e) if e.code == "empty_file"
        ));
        assert!(matches!(
            Outcome::from_result(Err(ProcessError::Internal("x"))),
            Outcome::Internal(_)
        ));
        assert_eq!(ErrorJson::too_complex().code, "too_complex");
    }

    #[test]
    fn enums_serialize_as_their_stable_names() {
        let name = |v: serde_json::Value| v.as_str().unwrap_or_default().to_string();
        for f in [FillRule::NonZero, FillRule::EvenOdd] {
            assert_eq!(name(serde_json::to_value(f).unwrap()), f.as_svg());
        }
        for s in [Strategy::Traced, Strategy::VectorExact, Strategy::Retraced] {
            assert_eq!(name(serde_json::to_value(s).unwrap()), s.as_str());
        }
        for i in [InputFormat::Png, InputFormat::Jpeg, InputFormat::Svg] {
            assert_eq!(name(serde_json::to_value(i).unwrap()), i.as_str());
        }
        for s in [Severity::Warn, Severity::Tip, Severity::Info] {
            assert_eq!(name(serde_json::to_value(s).unwrap()), s.as_str());
        }
        for f in [Fix::Flip, Fix::Fit] {
            assert_eq!(name(serde_json::to_value(f).unwrap()), f.as_str());
            assert_eq!(Fix::parse(f.as_str()), Some(f));
        }
        assert_eq!(Fix::parse("rotate"), None);
    }

    #[test]
    fn clean_path_alphabet() {
        assert!(is_clean_path_d("M0 100L100 0.5Q1 2 3 4C-1 2 3 4 5 6Z"));
        assert!(is_clean_path_d(""));
        for bad in ["M0 0\"", "M0 0<", "m0 0", "M0,0", "M0 0\n", "M0 0e5"] {
            assert!(!is_clean_path_d(bad), "{bad:?}");
        }
    }
}
