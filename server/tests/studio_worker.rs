//! The studio's upload worker, run as the real `arena studio-worker` child process.
//!
//! These prove the crash boundary from outside: a normal upload gives exactly the
//! in-process answer, and an abort, a memory bomb, a CPU hog and a hang each end only
//! the child, as the outcome the endpoint maps to a friendly response, while this
//! process (the "server") carries on.
//!
//! Linux only: they read `/proc` and rely on Linux's `RLIMIT_DATA`, which covers `mmap`.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arena::design_kit::{self, Fix, Limits};
use arena::studio_worker::{
    DEFAULT_ADDRESS_SPACE_LIMIT_BYTES, DEFAULT_DATA_LIMIT_BYTES, Outcome, Reply, SUBCOMMAND,
    ShapeJson, WORKER_NICE, Worker, WorkerLimits, exit,
};
use common::design_kit::{encode_jpeg, fixture, reference_chain, rgba_png};
use tokio::sync::Semaphore;

const ARENA: &str = env!("CARGO_BIN_EXE_arena");

fn worker() -> Worker {
    Worker::new(ARENA)
}

/// A head: full-height neck on the left, an eye hole, a mouth notch on the right.
fn head(x: f32, y: f32) -> bool {
    let eye = (x - 25.0).powi(2) + (y - 30.0).powi(2) < 64.0;
    let mouth = x > 50.0 && (y - 65.0).abs() < (x - 50.0) * 0.5;
    !(eye || mouth)
}

fn mirrored_head(x: f32, y: f32) -> bool {
    head(100.0 - x, y)
}

/// The head in black on white, as a JPEG.
fn head_jpeg(side: u32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((side * side * 3) as usize);
    for y in 0..side {
        for x in 0..side {
            let (u, v) = (
                (x as f32 + 0.5) * 100.0 / side as f32,
                (y as f32 + 0.5) * 100.0 / side as f32,
            );
            rgb.extend_from_slice(if head(u, v) { &[0; 3] } else { &[255; 3] });
        }
    }
    encode_jpeg(side as u16, side as u16, &rgb, 85)
}

fn expect_shape(outcome: Outcome) -> ShapeJson {
    match outcome {
        Outcome::Shape(shape) => *shape,
        other => panic!("expected a shape, got {other:?}"),
    }
}

/// The fields of `/proc/<pid>/stat` after the command name: index 0 is the state, 1
/// the parent pid, 16 the nice value.
fn stat_fields(pid: u32) -> Vec<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // "pid (comm) state ppid ...": comm may hold spaces, so read after the last ')'.
    stat.rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Running children of this process whose arguments are exactly `args` (`arena` and
/// then these). A killed worker drops out as soon as it dies: a zombie has no command
/// line. Tests run in parallel, so each uses its own arguments.
fn children_with(args: &[&str]) -> Vec<u32> {
    let me = std::process::id().to_string();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let argv: Vec<&[u8]> = cmdline
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .collect();
            stat_fields(pid).get(1) == Some(&me)
                && argv.len() == args.len() + 1
                && argv[1..].iter().zip(args).all(|(a, b)| *a == b.as_bytes())
        })
        .collect()
}

/// Wait up to 5 s for a child with exactly `args` to be running.
async fn wait_for_child(args: &[&str]) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(&pid) = children_with(args).first() {
            return Some(pid);
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn uploads_come_back_exactly_as_processed_in_process() {
    let cases = [
        ("png", rgba_png(512, head)),
        ("jpeg", head_jpeg(512)),
        ("svg", fixture("refs/heads/default.svg")),
    ];
    for (name, bytes) in cases {
        let expected = design_kit::process_upload(&bytes, &Limits::default(), &[])
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let shape = expect_shape(worker().run(&bytes, &[]).await);
        assert_eq!(shape, ShapeJson::from(&expected), "{name}");
    }
}

#[tokio::test]
async fn fixes_reach_the_worker() {
    let mirrored = rgba_png(512, mirrored_head);
    let plain = expect_shape(worker().run(&mirrored, &[]).await);
    let faces_left = plain.lints.head.iter().find(|l| l.code == "faces_left");
    assert_eq!(faces_left.and_then(|l| l.fix), Some(Fix::Flip), "{plain:?}");

    let flipped = expect_shape(worker().run(&mirrored, &[Fix::Flip]).await);
    assert!(
        flipped.lints.head.iter().all(|l| l.code != "faces_left"),
        "{:?}",
        flipped.lints.head
    );
    let expected = design_kit::process_upload(&mirrored, &Limits::default(), &[Fix::Flip])
        .expect("flipped in process");
    assert_eq!(flipped.path_d, expected.path_d());
}

#[tokio::test]
async fn upload_errors_come_back_as_rejections() {
    let mut big_svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\">".to_vec();
    big_svg.resize(600 * 1024, b' ');
    let cases: [(&str, Vec<u8>); 4] = [
        (
            "unsupported_format",
            b"8BPS\x00\x01\x00\x00\x00\x00\x00\x00".to_vec(),
        ),
        ("empty_file", Vec::new()),
        ("unknown_format", b"hello, world".to_vec()),
        ("too_large", big_svg),
    ];
    for (code, bytes) in cases {
        match worker().run(&bytes, &[]).await {
            Outcome::Rejected(error) => {
                assert_eq!(error.code, code);
                assert!(!error.message.is_empty());
            }
            other => panic!("{code}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_abort_kills_only_the_worker() {
    let started = Instant::now();
    match worker().with_self_test("abort").run(b"", &[]).await {
        Outcome::Crashed { status, .. } => assert_eq!(status.signal(), Some(libc::SIGABRT)),
        other => panic!("expected a crash, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    // This process is fine, and so is the next upload.
    expect_shape(worker().run(&rgba_png(256, head), &[]).await);
}

#[tokio::test]
async fn a_memory_bomb_dies_at_the_data_limit() {
    // The production limit: the bomb allocates and touches 16 MiB at a time, up to
    // 2 GiB, and exits with SELF_TEST_SURVIVED if nothing stops it.
    assert_eq!(WorkerLimits::default().data_bytes, DEFAULT_DATA_LIMIT_BYTES);
    let started = Instant::now();
    match worker().with_self_test("alloc").run(b"", &[]).await {
        Outcome::Crashed { status, stderr } => {
            assert_eq!(status.signal(), Some(libc::SIGABRT), "{stderr}");
            assert!(stderr.contains("memory allocation"), "{stderr}");
        }
        other => panic!("expected a crash, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    expect_shape(worker().run(&rgba_png(256, head), &[]).await);
}

#[tokio::test]
async fn real_processing_out_of_memory_is_a_crash() {
    // A legitimate 2048 px upload needs about 88 MiB (64 of them the processing
    // stack); at 80 MiB the decode runs out partway, as a bomb would under the real
    // limit, and the worker aborts instead of the server.
    let bytes = rgba_png(2048, head);
    let tight = WorkerLimits {
        data_bytes: 80 << 20,
        ..WorkerLimits::default()
    };
    match worker().with_limits(tight).run(&bytes, &[]).await {
        Outcome::Crashed { status, stderr } => {
            assert_eq!(status.signal(), Some(libc::SIGABRT), "{stderr}");
            assert!(stderr.contains("memory allocation"), "{stderr}");
        }
        other => panic!("expected a crash, got {other:?}"),
    }
    // With the default limit it's fine.
    expect_shape(worker().run(&bytes, &[]).await);
}

#[tokio::test]
async fn a_cpu_hog_dies_at_the_cpu_limit() {
    let limits = WorkerLimits {
        cpu_secs: 1,
        wall: Duration::from_secs(20),
        ..WorkerLimits::default()
    };
    let started = Instant::now();
    match worker()
        .with_self_test("spin")
        .with_limits(limits)
        .run(b"", &[])
        .await
    {
        // SIGXCPU at the soft limit (SIGKILL at the hard one, a second later).
        Outcome::Crashed { status, .. } => assert!(
            matches!(status.signal(), Some(libc::SIGXCPU | libc::SIGKILL)),
            "{status}"
        ),
        other => panic!("expected a crash, got {other:?}"),
    }
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
}

#[tokio::test]
async fn the_address_space_limit_stops_a_memory_bomb_on_its_own() {
    // gVisor (Cloud Run's first generation) applies RLIMIT_DATA to brk only, so a bomb
    // made of mmap'd allocations meets RLIMIT_AS alone. Lift the data limit to see it
    // hold by itself.
    assert_eq!(
        WorkerLimits::default().address_space_bytes,
        DEFAULT_ADDRESS_SPACE_LIMIT_BYTES
    );
    let as_only = WorkerLimits {
        data_bytes: libc::RLIM_INFINITY,
        ..WorkerLimits::default()
    };
    match worker()
        .with_self_test("alloc")
        .with_limits(as_only)
        .run(b"", &[])
        .await
    {
        Outcome::Crashed { status, stderr } => {
            assert_eq!(status.signal(), Some(libc::SIGABRT), "{stderr}");
            assert!(stderr.contains("memory allocation"), "{stderr}");
        }
        other => panic!("expected a crash, got {other:?}"),
    }
}

#[tokio::test]
async fn the_heaviest_legitimate_svgs_fit_the_default_limits() {
    // The deepest reference chains the caps allow (the most stack), and a 4,900-element
    // SVG that is rejected as too complex (the most heap before the answer).
    let rects: String = (0..4900)
        .map(|i| {
            format!(
                "<rect x=\"{}\" y=\"{}\" width=\"0.9\" height=\"0.9\" fill=\"#000000\" stroke=\"none\"/>",
                i % 100,
                i / 100
            )
        })
        .collect();
    let rects =
        format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\">{rects}</svg>");
    match worker().run(rects.as_bytes(), &[]).await {
        Outcome::Rejected(error) => assert_eq!(error.code, "too_complex"),
        other => panic!("4,900 rects: {other:?}"),
    }
    for (kind, links, nest) in [("pattern", 64, 60), ("mask", 64, 60), ("patuse", 33, 58)] {
        let svg = reference_chain(kind, links, nest);
        match worker().run(svg.as_bytes(), &[]).await {
            Outcome::Shape(_) => {}
            other => panic!("{kind} {links}x{nest}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_processing_slot_is_held_until_the_worker_is_gone() {
    let slot = Arc::new(Semaphore::new(1));
    let permit = slot.clone().try_acquire_owned().expect("a free slot");
    let args = [SUBCOMMAND, "--fix=flip", "--self-test=hang"];
    let hung = worker().with_self_test("hang").with_limits(WorkerLimits {
        wall: Duration::from_millis(1500),
        ..WorkerLimits::default()
    });
    let watch = async {
        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            // Permits first: if the worker is still running afterwards, it was running
            // (or not yet started) when they were read, so the slot must be taken.
            let free = slot.available_permits();
            if children_with(&args).is_empty() {
                if seen > 0 {
                    break;
                }
            } else {
                assert_eq!(free, 0, "the slot was released while the worker ran");
                seen += 1;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        seen
    };
    let (outcome, seen) = tokio::join!(hung.run_holding(b"", &[Fix::Flip], permit), watch);
    assert!(matches!(outcome, Outcome::TimedOut), "{outcome:?}");
    assert!(seen > 0, "never saw the worker run");
    assert_eq!(slot.available_permits(), 1, "the slot came back");
    assert!(children_with(&args).is_empty());
}

#[tokio::test]
async fn a_cancelled_request_kills_the_worker_and_frees_the_slot() {
    let slot = Arc::new(Semaphore::new(1));
    let permit = slot.clone().try_acquire_owned().expect("a free slot");
    let args = [SUBCOMMAND, "--fix=fit", "--self-test=hang"];
    // A deadline far away: only dropping the future can stop this worker in time.
    let hung = worker().with_self_test("hang").with_limits(WorkerLimits {
        wall: Duration::from_secs(60),
        ..WorkerLimits::default()
    });
    let run = hung.run_holding(b"", &[Fix::Fit], permit);
    tokio::select! {
        outcome = run => panic!("the hang ended on its own: {outcome:?}"),
        pid = wait_for_child(&args) => assert!(pid.is_some(), "the worker never started"),
    }
    // The request future is gone (the client went away).
    assert_eq!(slot.available_permits(), 1);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !children_with(&args).is_empty() {
        assert!(Instant::now() < deadline, "the worker outlived its request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_hung_worker_is_killed_at_the_deadline_with_its_limits_applied() {
    let args = [SUBCOMMAND, "--self-test=hang"];
    let wall = Duration::from_secs(2);
    let limits = WorkerLimits {
        wall,
        ..WorkerLimits::default()
    };
    let hung = worker().with_self_test("hang").with_limits(limits.clone());
    let started = Instant::now();
    let watch = async {
        // Find the worker while it hangs and read what the kernel says about it.
        let pid = wait_for_child(&args).await?;
        let read =
            |file: &str| std::fs::read_to_string(format!("/proc/{pid}/{file}")).unwrap_or_default();
        let nice = stat_fields(pid).get(16).cloned().unwrap_or_default();
        Some((
            pid,
            read("limits"),
            read("oom_score_adj"),
            read("environ"),
            nice,
        ))
    };
    let (outcome, seen) = tokio::join!(hung.run(b"", &[]), watch);
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Outcome::TimedOut), "{outcome:?}");
    assert!(
        elapsed >= wall && elapsed < wall + Duration::from_secs(3),
        "{elapsed:?}"
    );

    let (pid, proc_limits, oom, environ, nice) = seen.expect("the hung worker was running");
    let line = |name: &str| {
        proc_limits
            .lines()
            .find(|l| l.starts_with(name))
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    let cpu = limits.cpu_secs;
    assert_eq!(
        line("Max cpu time"),
        format!("Max cpu time {cpu} {} seconds", cpu + 1)
    );
    let data = limits.data_bytes;
    assert_eq!(
        line("Max data size"),
        format!("Max data size {data} {data} bytes")
    );
    let address_space = limits.address_space_bytes;
    assert_eq!(
        line("Max address space"),
        format!("Max address space {address_space} {address_space} bytes")
    );
    assert_eq!(line("Max core file size"), "Max core file size 0 0 bytes");
    assert_eq!(oom.trim(), "1000");
    assert_eq!(nice, WORKER_NICE.to_string(), "the lowest CPU priority");
    assert_eq!(environ, "", "the worker gets an empty environment");

    // Killed and reaped: gone, not a zombie.
    assert!(
        std::fs::metadata(format!("/proc/{pid}")).is_err(),
        "worker {pid} outlived the deadline"
    );
}

/// The stdin/stdout protocol and exit codes, without the parent's wrapper.
#[test]
fn the_worker_cli_speaks_json_with_documented_exit_codes() {
    let run = |args: &[&str], input: &[u8]| {
        let mut child = Command::new(ARENA)
            .arg(SUBCOMMAND)
            .args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the worker");
        let mut stdin = child.stdin.take().expect("stdin");
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().expect("worker output");
        writer.join().expect("writer thread");
        (out.status.code(), out.stdout)
    };

    let (code, stdout) = run(&["--fix=flip"], &rgba_png(256, mirrored_head));
    assert_eq!(code, Some(exit::REPLIED));
    match serde_json::from_slice(&stdout).expect("JSON reply") {
        Reply::Shape(shape) => assert!(shape.lints.head.iter().all(|l| l.code != "faces_left")),
        other => panic!("{other:?}"),
    }

    let (code, stdout) = run(&[], b"8BPS\x00\x01");
    assert_eq!(code, Some(exit::REPLIED));
    match serde_json::from_slice(&stdout).expect("JSON reply") {
        Reply::Error(error) => assert_eq!(error.code, "unsupported_format"),
        other => panic!("{other:?}"),
    }

    let (code, stdout) = run(&["--fix=rotate"], b"");
    assert_eq!(code, Some(exit::USAGE));
    assert!(stdout.is_empty());
}
