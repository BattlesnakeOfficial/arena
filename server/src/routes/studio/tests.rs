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
    BUCKET_CAPACITY, InProcess, MAX_BODY_BYTES, Processor, SLOT_WAIT, StudioState, TokenBucket,
    UPLOAD_SLOTS,
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
        let svg = json["svg"].as_str().expect("svg");
        let d = json["path_d"].as_str().expect("path_d");
        assert!(svg.starts_with("<svg"), "{input}: {svg}");
        assert!(svg.contains(d), "{input}");
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

#[test]
fn production_uses_a_worker_subprocess_with_the_planned_guards() {
    use super::process::{BUCKET_REFILL_PER_SEC, STUDIO_SLOTS};
    assert_eq!(UPLOAD_SLOTS, 3);
    assert_eq!(STUDIO_SLOTS, 1);
    assert_eq!(MAX_BODY_BYTES, 4 * 1024 * 1024);
    assert_eq!((BUCKET_CAPACITY, BUCKET_REFILL_PER_SEC), (20.0, 1.0));
    let studio = StudioState::subprocess().expect("current exe");
    assert_eq!(studio.upload_slots.available_permits(), UPLOAD_SLOTS);
    assert_eq!(studio.studio_slot.available_permits(), STUDIO_SLOTS);
    assert_eq!(studio.slot_wait, SLOT_WAIT);
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

    // 16 live frames + three four-snake boards: one head and one tail each.
    let heads = html.matches("class=\"studio-head\"").count();
    let tails = html.matches("class=\"studio-tail\"").count();
    assert_eq!((heads, tails), (16 + 3 * 4, 16 + 3 * 4));
    assert_eq!(html.matches("class=\"studio-frame\"").count(), 16);
    assert!(html.contains("id=\"studio-closeup-path\""));
    // Before any upload the slots show the default head and tail.
    let default_head = REFS
        .iter()
        .find(|r| r.kind == AssetKind::Head && r.slug == "default")
        .expect("default head");
    assert!(html.contains(&format!("d=\"{}\"", default_head.d)));
    assert!(html.contains("Try it with your own drawing."));

    // Every reference is offered for pairing, with a clean path.
    let ds = attr_values(&html, "data-d");
    assert_eq!(ds.len(), REFS.len());
    for d in ds {
        assert!(!d.is_empty() && is_clean_path_d(d), "{d}");
    }
    for rule in attr_values(&html, "data-fill-rule") {
        assert!(rule == "nonzero" || rule == "evenodd", "{rule}");
    }

    // The page's controls and script.
    for id in [
        "studio-file",
        "studio-status",
        "studio-top-warning",
        "studio-panes",
        "studio-pair-head",
        "studio-pair-tail",
        "studio-warnings",
        "studio-details",
        "studio-save-image",
        "studio-download-head",
        "studio-clear",
        "studio-result-heading",
    ] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{id}");
    }
    assert!(html.contains("aria-live=\"polite\""));
    assert!(html.contains("/static/studio.js?v="));
    assert!(html.contains("immediately discarded"));
    // No inline handlers.
    assert!(!html.contains(" onclick="));
    // Not linked from the nav or footer yet (DEV-1539 PR 4).
    assert!(!html.contains("href=\"/customizations/studio\""));
}
