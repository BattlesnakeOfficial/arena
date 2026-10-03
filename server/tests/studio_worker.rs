//! The studio's upload worker, run as the real `arena studio-worker` child process.
//!
//! These prove the crash boundary from outside: a normal upload gives exactly the
//! in-process answer, and an abort, a memory bomb, a CPU hog and a hang each end only
//! the child, as the outcome the endpoint maps to a friendly response, while this
//! process (the "server") carries on.

mod common;

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arena::design_kit::{self, Fix, Limits};
use arena::studio_worker::{
    DEFAULT_DATA_LIMIT_BYTES, Outcome, Reply, SUBCOMMAND, ShapeJson, Worker, WorkerLimits, exit,
};
use common::design_kit::{encode_jpeg, fixture, rgba_png};

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

/// Children of this process whose command line contains `needle`.
fn children_with(needle: &str) -> Vec<u32> {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // "pid (comm) state ppid ...": comm may hold spaces, so read after the ')'.
            let ppid = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|p| p.parse::<u32>().ok());
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            ppid == Some(me) && String::from_utf8_lossy(&cmdline).contains(needle)
        })
        .collect()
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
        assert!(shape.svg.starts_with("<svg"), "{name}");
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
async fn a_hung_worker_is_killed_at_the_deadline_with_its_limits_applied() {
    let needle = "--self-test=hang";
    let wall = Duration::from_secs(2);
    let limits = WorkerLimits {
        wall,
        ..WorkerLimits::default()
    };
    let hung = worker().with_self_test("hang").with_limits(limits.clone());
    let started = Instant::now();
    let watch = async {
        // Find the worker while it hangs and read what the kernel says about it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(&pid) = children_with(needle).first() {
                let read = |file: &str| {
                    std::fs::read_to_string(format!("/proc/{pid}/{file}")).unwrap_or_default()
                };
                return Some((pid, read("limits"), read("oom_score_adj"), read("environ")));
            }
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let (outcome, seen) = tokio::join!(hung.run(b"", &[]), watch);
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Outcome::TimedOut), "{outcome:?}");
    assert!(
        elapsed >= wall && elapsed < wall + Duration::from_secs(3),
        "{elapsed:?}"
    );

    let (pid, proc_limits, oom, environ) = seen.expect("the hung worker was running");
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
    assert_eq!(line("Max core file size"), "Max core file size 0 0 bytes");
    assert_eq!(oom.trim(), "1000");
    assert_eq!(environ, "", "the worker gets an empty environment");

    // Killed and reaped: gone, not a zombie.
    assert!(
        children_with(needle).is_empty(),
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
