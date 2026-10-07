//! Studio route tests, through the real router.
//!
//! The POST tests use a lazy pool pointing at a closed port, so any database access
//! would fail them: the processing route must never touch the database. Processing
//! runs in this process ([`InProcess`]) or through fakes; the worker subprocess has its
//! own tests in `server/tests/studio_worker.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use arena::design_kit::{AssetKind, Fix, refs::REFS};
use arena::studio_worker::{ErrorJson, Outcome, is_clean_path_d};
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode, header},
};
use color_eyre::eyre::eyre;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tower::ServiceExt as _;

use super::process::{
    BUCKET_CAPACITY, BodyPace, InProcess, MAX_BODY_BYTES, Processor, SLOT_WAIT, StudioState,
    TokenBucket, UPLOAD_SLOTS,
};
use crate::state::AppState;

const PROCESS: &str = "/customizations/studio/process";

/// An AppState whose database is unreachable.
fn db_free_state(studio: StudioState) -> AppState {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgresql://localhost:1/arena")
        .expect("lazy pool");
    let mut state = AppState::test_from_pool(pool);
    state.studio = studio;
    state
}

fn app(state: &AppState) -> Router {
    crate::routes::routes(state.clone())
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    json: serde_json::Value,
}

async fn post(app: Router, query: &str, body: Body) -> Reply {
    let request = Request::post(format!("{PROCESS}{query}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(body)
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Reply {
        status,
        headers,
        json,
    }
}

fn assert_no_store(reply: &Reply) {
    assert_eq!(
        reply
            .headers
            .get(header::CACHE_CONTROL)
            .map(|v| v.as_bytes()),
        Some(&b"no-store"[..]),
        "{}",
        reply.status
    );
}

fn assert_error(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.json);
    assert_eq!(reply.json["error"]["code"], code, "{}", reply.json);
    let message = reply.json["error"]["message"].as_str().unwrap_or_default();
    assert!(!message.is_empty(), "{}", reply.json);
    assert_no_store(reply);
}

// ---- fixtures -----------------------------------------------------------------------

/// A head: full-height neck on the left, an eye hole, a mouth notch on the right.
fn head(x: f32, y: f32) -> bool {
    let eye = (x - 25.0).powi(2) + (y - 30.0).powi(2) < 64.0;
    let mouth = x > 50.0 && (y - 65.0).abs() < (x - 50.0) * 0.5;
    !(eye || mouth)
}

/// Sample `ink(x, y)` (in 0..100 units) at pixel centres.
fn pixels(side: u32, ink: impl Fn(f32, f32) -> bool) -> impl Iterator<Item = bool> {
    let unit = 100.0 / side as f32;
    (0..side * side).map(move |i| {
        let (x, y) = (i % side, i / side);
        ink((x as f32 + 0.5) * unit, (y as f32 + 0.5) * unit)
    })
}

/// Black ink on a transparent background.
fn png(side: u32, ink: impl Fn(f32, f32) -> bool) -> Vec<u8> {
    let data: Vec<u8> = pixels(side, ink)
        .flat_map(|on| [0, 0, 0, if on { 255 } else { 0 }])
        .collect();
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, side, side);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&data).expect("png data");
    }
    out
}

/// Black ink on white.
fn jpeg(side: u32, ink: impl Fn(f32, f32) -> bool) -> Vec<u8> {
    let data: Vec<u8> = pixels(side, ink)
        .flat_map(|on| if on { [0; 3] } else { [255; 3] })
        .collect();
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 85)
        .encode(
            &data,
            side as u16,
            side as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("jpeg");
    out
}

const DEFAULT_HEAD_SVG: &str =
    include_str!("../../../tests/fixtures/design_kit/refs/heads/default.svg");

fn head_png() -> Vec<u8> {
    png(256, head)
}

/// A body that records whether anything read it.
fn tripwire_body() -> (Body, Arc<AtomicBool>) {
    let read = Arc::new(AtomicBool::new(false));
    let flag = read.clone();
    let stream = futures::stream::poll_fn(move |_| {
        flag.store(true, Ordering::SeqCst);
        Poll::Ready(Some(Ok::<_, std::io::Error>(Bytes::from_static(b"8BPS"))))
    });
    (Body::from_stream(stream), read)
}

// ---- fakes --------------------------------------------------------------------------

/// Answers with a fixed outcome.
struct Fixed(Mutex<Option<Outcome>>);

#[async_trait::async_trait]
impl Processor for Fixed {
    async fn process(&self, _: Vec<u8>, _: Vec<Fix>, _permit: OwnedSemaphorePermit) -> Outcome {
        self.0
            .lock()
            .expect("lock")
            .take()
            .unwrap_or(Outcome::Internal(eyre!("called twice")))
    }
}

/// Holds the permit until the test lets it finish.
struct Gate {
    started: Mutex<Option<oneshot::Sender<()>>>,
    finish: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl Processor for Gate {
    async fn process(&self, _: Vec<u8>, _: Vec<Fix>, permit: OwnedSemaphorePermit) -> Outcome {
        let started = self.started.lock().expect("lock").take();
        let finish = self.finish.lock().expect("lock").take();
        if let Some(started) = started {
            let _ = started.send(());
        }
        if let Some(finish) = finish {
            let _ = finish.await;
        }
        drop(permit);
        Outcome::Rejected(ErrorJson::new("empty", "done"))
    }
}

fn with_processor(processor: impl Processor + 'static) -> StudioState {
    StudioState::new(Arc::new(processor))
}

// ---- POST /customizations/studio/process --------------------------------------------

#[tokio::test]
async fn png_jpeg_and_svg_uploads_are_processed() {
    let state = db_free_state(StudioState::in_process());
    let cases = [
        ("png", head_png()),
        ("jpeg", jpeg(256, head)),
        ("svg", DEFAULT_HEAD_SVG.as_bytes().to_vec()),
    ];
    for (input, bytes) in cases {
        let reply = post(app(&state), "", Body::from(bytes)).await;
        assert_eq!(reply.status, StatusCode::OK, "{input}: {}", reply.json);
        assert_no_store(&reply);
        let json = &reply.json;
        assert_eq!(json["input"], input);
        let d = json["path_d"].as_str().expect("path_d");
        assert!(!d.is_empty() && is_clean_path_d(d), "{input}: {d}");
        assert!(["nonzero", "evenodd"].contains(&json["fill_rule"].as_str().unwrap_or("")));
        assert!(json["strategy"].is_string());
        assert!(json["metrics"]["left_edge_gaps"].is_array());
        assert!(json["lints"]["head"].is_array() && json["lints"]["tail"].is_array());
        assert!(json["info"].is_array());
    }
    // The drawn head passes every head check.
    let reply = post(app(&state), "", Body::from(head_png())).await;
    assert_eq!(reply.json["lints"]["head"], serde_json::json!([]));
}

#[tokio::test]
async fn upload_errors_are_422_with_their_code() {
    let state = db_free_state(StudioState::in_process());
    // Two-pixel black-and-white noise: a photo, not a drawing.
    let mut seed = 0x2545_f491_u32;
    let mut noise = move |_: f32, _: f32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed.is_multiple_of(2)
    };
    let noise_jpeg = {
        let side = 1024u32;
        let cells: Vec<bool> = (0..(side / 2) * (side / 2))
            .map(|_| noise(0.0, 0.0))
            .collect();
        jpeg(side, |x, y| {
            let (cx, cy) = ((x * 5.12) as u32, (y * 5.12) as u32);
            cells[(cy.min(511) * 512 + cx.min(511)) as usize]
        })
    };
    let cases: [(&str, Vec<u8>); 5] = [
        (
            "unsupported_format",
            b"8BPS\x00\x01\x00\x00\x00\x00".to_vec(),
        ),
        ("unsupported_format", b"PK\x03\x04procreate".to_vec()),
        ("empty_file", Vec::new()),
        ("unknown_format", b"just some text".to_vec()),
        ("too_complex", noise_jpeg),
    ];
    for (code, bytes) in cases {
        let reply = post(app(&state), "", Body::from(bytes)).await;
        assert_error(&reply, StatusCode::UNPROCESSABLE_ENTITY, code);
    }
}

#[tokio::test]
async fn oversize_uploads_are_413() {
    let state = db_free_state(StudioState::in_process());
    let reply = post(app(&state), "", Body::from(vec![0u8; MAX_BODY_BYTES + 1])).await;
    assert_error(&reply, StatusCode::PAYLOAD_TOO_LARGE, "too_large");
}

#[tokio::test]
async fn fix_flip_clears_faces_left() {
    let state = db_free_state(StudioState::in_process());
    let mirrored = png(256, |x, y| head(100.0 - x, y));
    let codes = |json: &serde_json::Value| -> Vec<String> {
        json["lints"]["head"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| l["code"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };

    let plain = post(app(&state), "", Body::from(mirrored.clone())).await;
    assert_eq!(plain.status, StatusCode::OK);
    assert!(
        codes(&plain.json).contains(&"faces_left".to_string()),
        "{}",
        plain.json
    );
    let faces_left = plain.json["lints"]["head"]
        .as_array()
        .and_then(|a| a.iter().find(|l| l["code"] == "faces_left"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(faces_left["fix"], "flip");
    assert_eq!(faces_left["severity"], "warn");

    for query in ["?fix=flip", "?fix=flip&fix=fit", "?fix=flip,fit"] {
        let fixed = post(app(&state), query, Body::from(mirrored.clone())).await;
        assert_eq!(fixed.status, StatusCode::OK, "{query}");
        assert!(codes(&fixed.json).is_empty(), "{query}: {}", fixed.json);
    }

    let bad = post(app(&state), "?fix=rotate", Body::from(mirrored)).await;
    assert_error(&bad, StatusCode::BAD_REQUEST, "bad_fix");
}

/// The SVG file studio.js builds from a saved path (`svgFor`): the download, and what
/// Flip and Fit re-post once the original file is gone (after a reload).
const PAGE_SVG: [&str; 3] = [
    "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><path fill-rule=\"",
    "\" d=\"",
    "\"/></svg>",
];

fn page_svg(fill_rule: &str, d: &str) -> String {
    format!(
        "{}{fill_rule}{}{d}{}",
        PAGE_SVG[0], PAGE_SVG[1], PAGE_SVG[2]
    )
}

fn head_codes(json: &serde_json::Value) -> Vec<String> {
    json["lints"]["head"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l["code"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn studio_js_builds_the_design_kit_svg_file() {
    let clean = arena::design_kit::process_upload(
        DEFAULT_HEAD_SVG.as_bytes(),
        &arena::design_kit::Limits::default(),
        &[],
    )
    .expect("the default head");
    assert_eq!(
        clean.to_svg(),
        page_svg(clean.fill_rule().as_svg(), clean.path_d())
    );
    let js = include_str!("../../../static/studio.js");
    for part in PAGE_SVG {
        let in_js = part.replace('"', "\\\"");
        assert!(
            js.contains(part) || js.contains(&in_js),
            "studio.js lost {part:?}"
        );
    }
}

#[tokio::test]
async fn fixes_work_on_the_svg_the_page_rebuilds_from_a_saved_path() {
    let state = db_free_state(StudioState::in_process());
    // A head drawn at 60% size in the middle of the left edge: Fit stretches it.
    let small = |x: f32, y: f32| {
        let (u, v) = (x / 0.6, (y - 20.0) / 0.6);
        (0.0..100.0).contains(&u) && (0.0..100.0).contains(&v) && head(u, v)
    };
    let cases = [
        ("flip", "faces_left", png(256, |x, y| head(100.0 - x, y))),
        ("fit", "margins", png(256, small)),
    ];
    for (fix, code, original) in cases {
        let plain = post(app(&state), "", Body::from(original.clone())).await;
        assert_eq!(plain.status, StatusCode::OK, "{fix}: {}", plain.json);
        let offered = plain.json["lints"]["head"]
            .as_array()
            .and_then(|a| a.iter().find(|l| l["code"] == code))
            .map(|l| l["fix"].clone());
        assert_eq!(offered, Some(serde_json::json!(fix)), "{}", plain.json);

        let saved = page_svg(
            plain.json["fill_rule"].as_str().unwrap_or_default(),
            plain.json["path_d"].as_str().unwrap_or_default(),
        );
        let query = format!("?fix={fix}");
        let from_saved = post(app(&state), &query, Body::from(saved)).await;
        let from_original = post(app(&state), &query, Body::from(original)).await;
        assert_eq!(
            from_saved.status,
            StatusCode::OK,
            "{fix}: {}",
            from_saved.json
        );
        assert_eq!(from_saved.json["input"], "svg");
        assert!(
            !head_codes(&from_saved.json).contains(&code.to_string()),
            "{fix}"
        );
        assert_eq!(
            head_codes(&from_saved.json),
            head_codes(&from_original.json),
            "{fix}"
        );
        let metrics = |r: &Reply| {
            let m = &r.json["metrics"];
            [
                m["fill_pct"].as_f64(),
                m["centroid"][0].as_f64(),
                m["centroid"][1].as_f64(),
                m["bbox"][0].as_f64(),
                m["bbox"][2].as_f64(),
            ]
        };
        for (a, b) in metrics(&from_saved)
            .into_iter()
            .zip(metrics(&from_original))
        {
            let (a, b) = (a.unwrap_or(-100.0), b.unwrap_or(100.0));
            assert!((a - b).abs() < 1.0, "{fix}: {a} vs {b}");
        }
    }
}

#[tokio::test]
async fn exhausted_upload_slots_answer_503_without_reading_the_body() {
    let state = db_free_state(StudioState::in_process());
    let held: Vec<_> = (0..UPLOAD_SLOTS)
        .map(|_| {
            state
                .studio
                .upload_slots
                .clone()
                .try_acquire_owned()
                .expect("slot")
        })
        .collect();
    let (body, read) = tripwire_body();
    let reply = post(app(&state), "", body).await;
    assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "busy");
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
    assert!(!read.load(Ordering::SeqCst), "the body was read");

    // A freed slot lets the next upload through.
    drop(held);
    let reply = post(app(&state), "", Body::from(head_png())).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn a_held_processing_slot_answers_503_after_the_wait() {
    assert_eq!(SLOT_WAIT, Duration::from_secs(3));
    let mut studio = StudioState::in_process();
    studio.slot_wait = Duration::from_millis(200);
    let state = db_free_state(studio);
    let _busy = state
        .studio
        .studio_slot
        .clone()
        .try_acquire_owned()
        .expect("slot");
    let started = Instant::now();
    let reply = post(app(&state), "", Body::from(head_png())).await;
    assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "busy");
    assert!(started.elapsed() >= Duration::from_millis(200));
    // The upload slot was given back.
    assert_eq!(state.studio.upload_slots.available_permits(), UPLOAD_SLOTS);
}

#[tokio::test]
async fn an_empty_bucket_answers_429_before_reading_the_body() {
    let mut studio = StudioState::in_process();
    let frozen = Instant::now();
    studio.clock = Arc::new(move || frozen);
    studio.bucket = Arc::new(Mutex::new(TokenBucket::new(BUCKET_CAPACITY, 1.0)));
    let state = db_free_state(studio);
    // Spend the burst at one instant; the frozen clock never refills it.
    for _ in 0..BUCKET_CAPACITY as usize {
        let reply = post(app(&state), "", Body::from(b"8BPS".to_vec())).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let (body, read) = tripwire_body();
    let reply = post(app(&state), "", body).await;
    assert_error(&reply, StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
    assert!(!read.load(Ordering::SeqCst), "the body was read");
}

#[test]
fn the_token_bucket_refills_with_the_clock() {
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut bucket = TokenBucket::new(2.0, 1.0);
    assert!(bucket.try_take(at(0)));
    assert!(bucket.try_take(at(0)));
    assert!(!bucket.try_take(at(0)));
    assert!(!bucket.try_take(at(500)));
    assert!(bucket.try_take(at(1000)));
    assert!(!bucket.try_take(at(1000)));
    // Never more than the capacity, however long it waits.
    assert!(bucket.try_take(at(60_000)));
    assert!(bucket.try_take(at(60_000)));
    assert!(!bucket.try_take(at(60_000)));
    // A clock that goes backwards doesn't refill.
    assert!(!bucket.try_take(at(30_000)));
}

#[tokio::test]
async fn the_processing_slot_is_held_while_processing() {
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let state = db_free_state(with_processor(Gate {
        started: Mutex::new(Some(started_tx)),
        finish: Mutex::new(Some(finish_rx)),
    }));
    let request = tokio::spawn(post(app(&state), "", Body::from(head_png())));
    started_rx.await.expect("processing started");
    assert_eq!(state.studio.studio_slot.available_permits(), 0);
    assert_eq!(
        state.studio.upload_slots.available_permits(),
        UPLOAD_SLOTS - 1
    );
    finish_tx.send(()).expect("still processing");
    let reply = request.await.expect("request task");
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(state.studio.studio_slot.available_permits(), 1);
    assert_eq!(state.studio.upload_slots.available_permits(), UPLOAD_SLOTS);
}

#[tokio::test]
async fn in_process_timeouts_keep_the_slot_until_the_work_ends() {
    // A deadline shorter than any real processing: the handler answers busy while the
    // big-stack thread still holds the slot, and the slot comes back when it finishes.
    let mut studio = StudioState::new(Arc::new(InProcess {
        limits: arena::design_kit::Limits::default(),
        deadline: Duration::ZERO,
    }));
    studio.slot_wait = Duration::from_millis(100);
    let state = db_free_state(studio);
    let reply = post(app(&state), "", Body::from(png(2048, head))).await;
    assert_error(&reply, StatusCode::SERVICE_UNAVAILABLE, "busy");
    assert_eq!(
        state.studio.studio_slot.available_permits(),
        0,
        "released while the thread still runs"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while state.studio.studio_slot.available_permits() == 0 {
        assert!(Instant::now() < deadline, "the slot never came back");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn worker_failures_map_to_friendly_responses() {
    use std::os::unix::process::ExitStatusExt as _;
    let crash = Outcome::Crashed {
        status: std::process::ExitStatus::from_raw(libc::SIGABRT),
        stderr: "memory allocation of 16777216 bytes failed".into(),
    };
    let cases = [
        (crash, StatusCode::UNPROCESSABLE_ENTITY, Some("too_complex")),
        (
            Outcome::TimedOut,
            StatusCode::SERVICE_UNAVAILABLE,
            Some("busy"),
        ),
        (
            Outcome::Rejected(ErrorJson::new("invalid_svg", "We couldn't read that SVG.")),
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("invalid_svg"),
        ),
        (
            Outcome::Internal(eyre!("bad studio worker reply")),
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
        ),
    ];
    for (outcome, status, code) in cases {
        let state = db_free_state(with_processor(Fixed(Mutex::new(Some(outcome)))));
        let reply = post(app(&state), "", Body::from(head_png())).await;
        match code {
            Some(code) => assert_error(&reply, status, code),
            None => {
                assert_eq!(reply.status, status);
                assert_no_store(&reply);
            }
        }
        assert_eq!(state.studio.studio_slot.available_permits(), 1);
    }
}

// ---- GET /customizations/studio ----------------------------------------------------

fn attr_values<'a>(html: &'a str, attr: &str) -> Vec<&'a str> {
    let needle = format!("{attr}=\"");
    html.match_indices(&needle)
        .filter_map(|(i, _)| {
            let rest = &html[i + needle.len()..];
            rest.find('"').map(|end| &rest[..end])
        })
        .collect()
}

#[sqlx::test(migrations = "../migrations")]
async fn the_page_renders_every_board_with_placeholders(db: sqlx::PgPool) {
    let state = AppState::test_from_pool(db);
    let app = crate::routes::routes(state).layer(tower_cookies::CookieManagerLayer::new());
    let response = app
        .oneshot(
            Request::get("/customizations/studio")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let html = String::from_utf8(bytes.to_vec()).expect("utf-8");

    // 16 live frames + three four-snake boards: one head and one tail each, every one
    // showing the default head or tail before any upload.
    let default = |kind| {
        REFS.iter()
            .find(|r| r.kind == kind && r.slug == "default")
            .expect("a default reference")
    };
    let (default_head, default_tail) = (default(AssetKind::Head), default(AssetKind::Tail));
    for (class, shape) in [("studio-head", default_head), ("studio-tail", default_tail)] {
        let placeholder = format!(
            "<path class=\"{class}\" d=\"{}\" fill-rule=\"{}\">",
            shape.d,
            shape.fill_rule.as_svg()
        );
        assert_eq!(html.matches(&placeholder).count(), 16 + 3 * 4, "{class}");
        assert_eq!(
            html.matches(&format!("class=\"{class}\"")).count(),
            16 + 3 * 4,
            "{class}"
        );
    }
    assert_eq!(html.matches("class=\"studio-frame\"").count(), 16);
    assert!(html.contains("Try it with your own drawing."));

    // "Your snake": a card per slot, each with its own close-up, thumbnail and checks,
    // every one showing that slot's default before any upload. No "Upload as" toggle:
    // each card's file input fills its own slot.
    for (kind, shape) in [("head", default_head), ("tail", default_tail)] {
        for part in ["closeup", "thumb"] {
            let path = format!(
                "<path id=\"studio-{part}-path-{kind}\" class=\"studio-{part}-path\" d=\"{}\" fill-rule=\"{}\">",
                shape.d,
                shape.fill_rule.as_svg()
            );
            assert!(html.contains(&path), "{path}");
        }
        assert!(html.contains(&format!("aria-label=\"Close-up of the default {kind}\"")));
        let file = format!(
            "<input id=\"studio-file-{kind}\" class=\"studio-file\" type=\"file\" name=\"studio-file-{kind}\" data-kind=\"{kind}\""
        );
        assert!(html.contains(&file), "{file}");
        assert!(html.contains(&format!(">Upload {kind}</span>")), "{kind}");
        assert!(html.contains(&format!(">Default {kind}</p>")), "{kind}");
        for part in [
            "slot",
            "thumb",
            "name",
            "upload",
            "upload-text",
            "style",
            "download",
            "remove",
            "relabel",
            "undo",
            "confirm",
            "confirm-yes",
            "confirm-no",
            "closeup",
            "gaps",
            "checks",
            "pass",
            "warnings",
            "tips",
            "details",
            "details-summary",
            "info",
        ] {
            // The leading space keeps `data-testid="…"` from counting.
            let id = format!(" id=\"studio-{part}-{kind}\"");
            assert_eq!(html.matches(&id).count(), 1, "{id}");
        }
        assert!(html.contains(&format!("data-testid=\"studio-checks-{kind}\"")));
        assert!(html.contains(&format!("Passes every check the official {kind}s pass.")));
    }
    assert!(html.contains(">This is actually a tail</button>"));
    assert!(html.contains(">This is actually a head</button>"));
    assert!(html.contains(">Replace your tail</button>"));
    for gone in [
        "name=\"studio-kind\"",
        "Upload as",
        "Pair your",
        "studio-pair",
    ] {
        assert!(!html.contains(gone), "{gone}");
    }

    // Every reference is offered as a style for its slot, with a clean path.
    let ds = attr_values(&html, "data-d");
    assert_eq!(ds.len(), REFS.len());
    for d in ds {
        assert!(!d.is_empty() && is_clean_path_d(d), "{d}");
    }
    for rule in attr_values(&html, "data-fill-rule") {
        assert!(rule == "nonzero" || rule == "evenodd", "{rule}");
    }
    for (kind, select) in [
        (AssetKind::Head, "studio-style-head"),
        (AssetKind::Tail, "studio-style-tail"),
    ] {
        let start = html.find(&format!("id=\"{select}\"")).expect(select);
        let end = start + html[start..].find("</select>").expect("</select>");
        let options = &html[start..end];
        let count = REFS.iter().filter(|r| r.kind == kind).count();
        assert_eq!(options.matches("<option ").count(), count, "{select}");
        assert!(options.contains("value=\"default\""), "{select}");
    }

    // The page's controls and script.
    for id in [
        "studio-snake",
        "studio-slots",
        "studio-summary",
        "studio-status",
        "studio-top-warning",
        "studio-top-fix",
        "studio-panes",
        "studio-closeup",
        "studio-lints-empty",
        "studio-save-image",
        "studio-result-heading",
    ] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{id}");
    }
    assert!(html.contains("aria-live=\"polite\""));
    assert!(html.contains("/static/studio.js?v="));
    assert!(html.contains("immediately discarded"));
    // No inline handlers.
    assert!(!html.contains(" onclick="));

    // Start here: open before any upload, with both templates for each kind (labelled
    // by app), the guide, and the example studio.js posts as a head.
    assert!(html.contains("<details id=\"studio-start\""));
    assert!(html.contains(" open>"));
    // Under "Your snake", so both upload buttons come first on a first visit too.
    let snake_at = html.find("id=\"studio-snake\"").expect("Your snake");
    let start_at = html.find("id=\"studio-start\"").expect("Start here");
    let preview_at = html.find("id=\"studio-result-heading\"").expect("Preview");
    assert!(snake_at < start_at && start_at < preview_at);
    assert_eq!(html.matches(">Procreate (PSD)</a>").count(), 2);
    assert_eq!(
        html.matches(">Illustrator · Inkscape · Affinity (SVG)</a>")
            .count(),
        2
    );
    assert!(html.contains(&format!("href=\"{}\"", super::guide::GUIDE_PATH)));
    assert!(html.contains(&format!("data-guide=\"{}\"", super::guide::GUIDE_PATH)));
    let example = attr_values(&html, "data-src");
    assert_eq!(example.len(), 1);
    assert!(
        example[0].starts_with("/static/design-kit/example-drawing.png?v="),
        "{example:?}"
    );
}

// ---- GET /customizations/studio/guide, downloads, links in -------------------------

async fn get_html(app: Router, path: &str) -> (StatusCode, HeaderMap, String) {
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Every `<a>` tag (the whole start tag) with a `download` attribute.
fn download_tags(html: &str) -> Vec<&str> {
    html.match_indices("<a ")
        .filter_map(|(i, _)| {
            let tag = &html[i..i + html[i..].find('>')?];
            tag.contains(" download=\"").then_some(tag)
        })
        .collect()
}

fn page_app(db: sqlx::PgPool) -> Router {
    let state = AppState::test_from_pool(db);
    crate::routes::routes(state).layer(tower_cookies::CookieManagerLayer::new())
}

#[sqlx::test(migrations = "../migrations")]
async fn the_guide_renders_every_section_the_checks_link_to(db: sqlx::PgPool) {
    use super::guide::{GUIDE_PATH, RULE_ANCHORS};
    let (status, _, html) = get_html(page_app(db), GUIDE_PATH).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<h1>Make your own head &amp; tail</h1>"));
    assert!(html.contains("<meta name=\"description\" content=\"How to draw a Battlesnake"));
    for id in RULE_ANCHORS.iter().chain(&[
        "templates",
        "anatomy",
        "first-head",
        "procreate",
        "vector",
        "next",
    ]) {
        assert!(html.contains(&format!("id=\"{id}\"")), "#{id}");
    }
    // Every anchor a check links to is a section of the guide.
    for lint in every_lint() {
        let anchor = lint.guide_anchor();
        assert!(
            RULE_ANCHORS.contains(&anchor.trim_start_matches('#')),
            "{} links to {anchor}",
            lint.code()
        );
    }
    // The illustrations: the default head and tail on a real board, and close up.
    let default = |kind| {
        REFS.iter()
            .find(|r| r.kind == kind && r.slug == "default")
            .expect("a default reference")
    };
    for kind in [AssetKind::Head, AssetKind::Tail] {
        assert!(
            html.contains(&format!("d=\"{}\"", default(kind).d)),
            "{kind:?}"
        );
    }
    assert!(html.contains("class=\"studio-board guide-directions light\""));
    assert_eq!(html.matches("class=\"guide-closeup\"").count(), 2);
    // Four templates (two per kind) and the guides-only PNGs, each a download.
    assert_eq!(download_tags(&html).len(), 6);
    // The Procreate steps the artist needs.
    for step in [
        "Files → Downloads",
        "<strong>Import</strong>",
        "Insert a file",
        "Studio Pen",
        "Monoline",
        "Erase for holes",
        "Hide Guides and Reference; leave Background on.",
        "Actions (wrench) → Share → PNG → Save to Files",
    ] {
        assert!(html.contains(step), "{step}");
    }
    assert!(html.contains("href=\"/discord\""));
    assert!(html.contains("href=\"/customizations/studio\""));
    assert!(html.contains("keep every detail and every gap at least 40 px"));
}

/// One of every lint (the values don't matter, only the variant), as a chain: each
/// variant's arm names the next one. The match has no wildcard, so a new variant doesn't
/// compile until it has an arm, and it's only listed once an arm names it as the next.
fn every_lint() -> Vec<arena::design_kit::Lint> {
    use arena::design_kit::{Edge, Lint};
    let mut all = Vec::new();
    let mut next = Some(Lint::NearlyEmpty { fill_pct: 1.0 });
    while let Some(lint) = next {
        next = match &lint {
            Lint::NearlyEmpty { .. } => Some(Lint::SolidSquare { fill_pct: 99.0 }),
            Lint::SolidSquare { .. } => Some(Lint::NeckGap {
                left_edge_pct: 50.0,
            }),
            Lint::NeckGap { .. } => Some(Lint::Margins {
                bbox: [10.0, 10.0, 90.0, 90.0],
            }),
            Lint::Margins { .. } => Some(Lint::FacesLeft { centroid_x: 60.0 }),
            Lint::FacesLeft { .. } => Some(Lint::FacesUpDown {
                right_edge_pct: 90.0,
            }),
            Lint::FacesUpDown { .. } => Some(Lint::TailReversed {
                attach_edge: Edge::Right,
            }),
            Lint::TailReversed { .. } => Some(Lint::OutlineOnly { hole_pct: 80.0 }),
            Lint::OutlineOnly { .. } => Some(Lint::SpecksRemoved { count: 2 }),
            Lint::SpecksRemoved { .. } => Some(Lint::ColoursFlattened),
            Lint::ColoursFlattened => Some(Lint::GuidesVisible),
            Lint::GuidesVisible => Some(Lint::OutsideDrawHereIgnored),
            Lint::OutsideDrawHereIgnored => Some(Lint::SemiTransparent),
            Lint::SemiTransparent => Some(Lint::NonSquare {
                width: 10,
                height: 20,
            }),
            Lint::NonSquare { .. } => Some(Lint::LowResolution {
                width: 10,
                height: 10,
            }),
            Lint::LowResolution { .. } => Some(Lint::StrokesConverted { count: 1 }),
            Lint::StrokesConverted { .. } => Some(Lint::Gradient),
            Lint::Gradient => Some(Lint::ImageIgnored { count: 1 }),
            Lint::ImageIgnored { .. } => Some(Lint::TextIgnored),
            Lint::TextIgnored => Some(Lint::ClipOrMask {
                clipped: true,
                masked: false,
            }),
            Lint::ClipOrMask { .. } => Some(Lint::FiltersIgnored),
            Lint::FiltersIgnored => Some(Lint::ActiveContentRemoved),
            Lint::ActiveContentRemoved => Some(Lint::OutsideCanvas),
            Lint::OutsideCanvas => None,
        };
        all.push(lint);
    }
    let codes: std::collections::BTreeSet<_> = all.iter().map(|l| l.code()).collect();
    assert_eq!(codes.len(), all.len(), "each variant once");
    all
}

#[sqlx::test(migrations = "../migrations")]
async fn the_studio_hands_its_script_a_topic_for_every_guide_section(db: sqlx::PgPool) {
    use super::guide::{RULE_ANCHORS, RULE_TOPICS};
    // studio.js reads "Learn more about …" from data-guide-topics, so every section a
    // check links to gets a link.
    let (status, _, html) = get_html(page_app(db), "/customizations/studio").await;
    assert_eq!(status, StatusCode::OK);
    let json = serde_json::to_string(&RULE_TOPICS).expect("json");
    let attr = format!("data-guide-topics=\"{}\"", json.replace('"', "&quot;"));
    assert!(html.contains(&attr), "{attr}");
    for lint in every_lint() {
        let anchor = lint.guide_anchor().trim_start_matches('#');
        assert!(RULE_ANCHORS.contains(&anchor), "{}", lint.code());
    }
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/studio.js"),
    )
    .expect("studio.js");
    assert!(script.contains("data-guide-topics"));
}

#[sqlx::test(migrations = "../migrations")]
async fn every_design_kit_link_downloads_the_committed_file(db: sqlx::PgPool) {
    let app = page_app(db);
    let static_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    let mut checked = std::collections::BTreeSet::new();
    for page in ["/customizations/studio", super::guide::GUIDE_PATH] {
        let (status, _, html) = get_html(app.clone(), page).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let mut links: Vec<(String, Option<String>)> = download_tags(&html)
            .into_iter()
            .map(|tag| {
                let href = attr_values(tag, "href").first().map(|h| h.to_string());
                let name = attr_values(tag, "download").first().map(|d| d.to_string());
                (href.expect("download links have an href"), name)
            })
            .collect();
        // "Try an example" fetches its drawing the same way.
        links.extend(
            attr_values(&html, "data-src")
                .into_iter()
                .map(|src| (src.to_string(), None)),
        );
        assert!(links.len() >= 5, "{page}: {links:?}");
        for (href, name) in links {
            // asset_url: the file exists in the embedded static dir (so it carries a
            // content hash), and the download name is the file's own.
            let (path, version) = href
                .strip_prefix("/static/")
                .and_then(|rest| rest.split_once("?v="))
                .unwrap_or_else(|| panic!("{page}: {href} is not a versioned asset_url"));
            assert!(path.starts_with("design-kit/"), "{href}");
            assert_eq!(version.len(), 16, "{href}");
            if let Some(name) = name {
                assert_eq!(path.rsplit('/').next(), Some(name.as_str()), "{href}");
            }
            let on_disk = std::fs::read(static_dir.join(path))
                .unwrap_or_else(|e| panic!("{page}: {path}: {e}"));
            let response = app
                .clone()
                .oneshot(Request::get(&href).body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{href}");
            let served = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            assert!(served == on_disk, "{href} serves the committed file");
            checked.insert(path.to_string());
        }
    }
    // Every file in the kit is offered somewhere.
    let kit: std::collections::BTreeSet<String> = std::fs::read_dir(static_dir.join("design-kit"))
        .expect("static/design-kit")
        .map(|e| {
            format!(
                "design-kit/{}",
                e.expect("entry").file_name().to_string_lossy()
            )
        })
        .collect();
    assert_eq!(checked, kit);
}

#[tokio::test]
async fn studio_redirects_to_the_studio() {
    let app = app(&db_free_state(StudioState::in_process()));
    for (path, location) in [
        ("/studio", "/customizations/studio"),
        (
            "/studio?utm_source=discord",
            "/customizations/studio?utm_source=discord",
        ),
    ] {
        let (status, headers, _) = get_html(app.clone(), path).await;
        assert!(status.is_redirection(), "{path}: {status}");
        assert_eq!(
            headers.get(header::LOCATION).map(|v| v.as_bytes()),
            Some(location.as_bytes()),
            "{path}"
        );
    }
}

/// The studio isn't advertised yet (DEV-1539): it's reachable by URL (and `/studio`), but
/// neither `/customizations` nor the site footer links to it. Linking it publicly is a
/// launch decision, so adding those links back has to change this test on purpose.
#[sqlx::test(migrations = "../migrations")]
async fn the_studio_is_not_linked_from_customizations_or_the_footer_yet(db: sqlx::PgPool) {
    use maud::Render as _;

    let (status, _, customizations) = get_html(page_app(db), "/customizations").await;
    assert_eq!(status, StatusCode::OK);
    let footer = crate::components::page::Page::new(
        "Test".to_string(),
        Box::new(maud::html! { p { "content" } }),
        None,
    )
    .render()
    .into_string();

    for (what, html) in [
        ("/customizations", &customizations),
        ("the footer", &footer),
    ] {
        // A whole page, footer included, so the checks below can fail.
        assert!(html.contains("class=\"site-footer\""), "{what}: no footer");
        assert!(html.contains("href=\"/terms\""), "{what}: no footer links");
        for href in ["href=\"/customizations/studio", "href=\"/studio"] {
            assert!(!html.contains(href), "{what} links to the studio ({href})");
        }
    }
    assert!(customizations.contains("Reach out on Discord"));
}

/// The page's in-browser check, run the way studio.js runs it: the first rejected
/// signature whose parts all match gives its message, else nothing (the file is posted).
fn client_check(sniff: &serde_json::Value, bytes: &[u8]) -> Option<String> {
    let rejected = sniff["rejected"].as_array()?;
    rejected
        .iter()
        .find(|sig| {
            sig["at"].as_array().is_some_and(|parts| {
                parts.iter().all(|part| {
                    let offset = part[0].as_u64().and_then(|o| usize::try_from(o).ok());
                    part[1].as_array().is_some_and(|alternatives| {
                        alternatives.iter().any(|magic| {
                            magic.as_array().is_some_and(|magic| {
                                magic.iter().enumerate().all(|(i, v)| {
                                    let at = offset.and_then(|o| o.checked_add(i));
                                    at.and_then(|j| bytes.get(j)).map(|&b| u64::from(b))
                                        == v.as_u64()
                                })
                            })
                        })
                    })
                })
            })
        })
        .and_then(|sig| sig["message"].as_str().map(String::from))
}

#[test]
fn the_browser_check_gives_the_server_answer_for_every_rejected_format() {
    use arena::design_kit::{ProcessError, sniff};
    let html = super::page::studio_markup().into_string();
    let attr = attr_values(&html, "data-sniff");
    assert_eq!(attr.len(), 1);
    // Maud escapes the attribute; the browser unescapes it before JSON.parse.
    let json = attr[0]
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    let sniff_table: serde_json::Value = serde_json::from_str(&json).expect("data-sniff is JSON");
    assert_eq!(sniff_table["max_bytes"], MAX_BODY_BYTES);
    let too_large = ProcessError::TooLarge {
        bytes: MAX_BODY_BYTES + 1,
        max: MAX_BODY_BYTES,
    };
    assert_eq!(sniff_table["too_large"], too_large.user_message());

    let utf16: Vec<u8> = "<svg/>".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let samples: Vec<(&str, Vec<u8>)> = vec![
        ("heic", b"\0\0\0\x18ftypheic\0\0\0\0".to_vec()),
        ("avif", b"\0\0\0\x18ftypavif\0\0\0\0".to_vec()),
        ("mp4 video", b"\0\0\0\x18ftypisom\0\0\0\0".to_vec()),
        ("gif", b"GIF89a\x01\0".to_vec()),
        ("webp", b"RIFF\0\0\0\0WEBPVP8 ".to_vec()),
        ("webp without riff", b"XXXX\0\0\0\0WEBPVP8 ".to_vec()),
        ("psd", b"8BPS\0\x01".to_vec()),
        ("procreate", b"PK\x03\x04procreate".to_vec()),
        ("pdf", b"%PDF-1.7".to_vec()),
        ("svgz", vec![0x1f, 0x8b, 0x08, 0x00]),
        ("utf-16 svg", utf16),
        ("png", head_png()),
        ("jpeg", jpeg(16, head)),
        ("svg", DEFAULT_HEAD_SVG.as_bytes().to_vec()),
        ("text", b"just some text".to_vec()),
    ];
    for (name, bytes) in samples {
        let server = match sniff(&bytes) {
            Err(e @ ProcessError::UnsupportedFormat(_)) => Some(e.user_message()),
            _ => None,
        };
        assert_eq!(client_check(&sniff_table, &bytes), server, "{name}");
    }
}

// ---- slow uploads ---------------------------------------------------------------------

/// A body that sends `first`, then `trickle` bytes every `every`, forever (or nothing
/// more if `every` is `None`).
fn slow_body(first: &'static [u8], every: Option<Duration>) -> Body {
    let start =
        futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from_static(first)) });
    let rest = futures::stream::unfold((), move |()| async move {
        match every {
            Some(every) => {
                tokio::time::sleep(every).await;
                Some((Ok(Bytes::from_static(b" ")), ()))
            }
            None => futures::future::pending().await,
        }
    });
    Body::from_stream(futures::StreamExt::chain(start, rest))
}

fn quick_pace() -> BodyPace {
    BodyPace {
        deadline: Duration::from_secs(20),
        grace: Duration::from_millis(300),
        min_bytes_per_sec: 1024,
    }
}

#[tokio::test]
async fn a_stalled_upload_is_cut_off_and_frees_its_slot() {
    let mut studio = StudioState::in_process();
    studio.body_pace = quick_pace();
    let state = db_free_state(studio);
    let started = Instant::now();
    let reply = post(app(&state), "", slow_body(b"8BPS", None)).await;
    assert_error(&reply, StatusCode::REQUEST_TIMEOUT, "upload_timeout");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(state.studio.upload_slots.available_permits(), UPLOAD_SLOTS);
}

#[tokio::test]
async fn a_trickling_upload_is_cut_off_long_before_the_deadline() {
    // A byte every 100 ms keeps the connection busy but is far below 1 KiB/s.
    let mut studio = StudioState::in_process();
    studio.body_pace = quick_pace();
    let state = db_free_state(studio);
    let started = Instant::now();
    let reply = post(
        app(&state),
        "",
        slow_body(b"8BPS", Some(Duration::from_millis(100))),
    )
    .await;
    assert_error(&reply, StatusCode::REQUEST_TIMEOUT, "upload_timeout");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(state.studio.upload_slots.available_permits(), UPLOAD_SLOTS);
}

#[tokio::test]
async fn an_upload_in_steady_chunks_gets_through() {
    // Twenty chunks 40 ms apart: longer than the grace period in total, and well above
    // 1 KiB/s.
    let mut studio = StudioState::in_process();
    studio.body_pace = quick_pace();
    let state = db_free_state(studio);
    let bytes = png(1024, head);
    let chunks: Vec<Bytes> = bytes
        .chunks(bytes.len().div_ceil(20))
        .map(Bytes::copy_from_slice)
        .collect();
    assert!(chunks.len() >= 10 && bytes.len() > 1024, "{}", bytes.len());
    let stream = futures::stream::unfold(chunks.into_iter(), |mut chunks| async move {
        let chunk = chunks.next()?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Some((Ok::<_, std::io::Error>(chunk), chunks))
    });
    let reply = post(app(&state), "", Body::from_stream(stream)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    assert_eq!(reply.json["input"], "png");
}
