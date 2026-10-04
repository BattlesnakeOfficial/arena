//! `POST /customizations/studio/process?fix=flip&fix=fit`: one upload (the raw request
//! body) in, one clean head or tail path (or a friendly error) out.
//!
//! The studio is public and shares one small instance with the live game engine, so the
//! route is guarded in this order, cheapest first:
//!
//! 1. [`upload_slot_guard`] (route middleware): at most [`UPLOAD_SLOTS`] requests past
//!    this point, taken with `try_acquire` **before** the body is read; otherwise 503
//!    `busy`. Caps the memory held in request bodies at 3 x 4 MiB.
//! 2. A global token bucket ([`TokenBucket`], [`BUCKET_CAPACITY`] refilling
//!    [`BUCKET_REFILL_PER_SEC`]): 429 `rate_limited`, still before the body is read.
//!    Global because `X-Forwarded-For` can't be trusted on the `run.app` URL.
//! 3. The body, up to [`MAX_BODY_BYTES`] (`DefaultBodyLimit`): 413 `too_large`. It must
//!    keep arriving ([`BodyPace`]): a client that stalls or trickles is cut off with 408
//!    `upload_timeout` within seconds, so it can't hold an upload slot for free.
//! 4. [`STUDIO_SLOTS`] processing slot, waited for up to [`SLOT_WAIT`]: 503 `busy`.
//! 5. The [`Processor`]: in production a short-lived `arena studio-worker` child
//!    ([`arena::studio_worker`]) with CPU and memory limits, killed at the wall-clock
//!    deadline [`arena::studio_worker::WorkerLimits::wall`] (10 s in release). The slot
//!    is held until the worker has exited.
//!
//! No `PageFactory`, `OptionalUser` or session: the route never touches the database.
//! Every response is `Cache-Control: no-store`, and the log never holds content or IPs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arena::design_kit::{self, Fix, Limits, ProcessError};
use arena::studio_worker::{ErrorJson, Outcome, Worker, WorkerLimits};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, FromRequest, RawQuery, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{MethodRouter, post},
};
use color_eyre::eyre::{Context as _, eyre};
use futures::StreamExt as _;
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{errors::ServerResult, state::AppState};

/// Requests allowed past the first guard at once (receiving a body or processing).
/// Taken before the body is read, so at most 3 x [`MAX_BODY_BYTES`] of uploads are in
/// memory.
pub const UPLOAD_SLOTS: usize = 3;

/// Uploads processed at once. Like `BACKUP_SLOT` in `backup.rs`: production has one
/// vCPU shared with live games, and processing is CPU-bound, so it runs one at a time.
pub const STUDIO_SLOTS: usize = 1;

/// Largest request body: the largest per-format cap (PNG/JPEG; SVGs stop at 512 KiB).
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How long an upload waits for the processing slot before giving up with 503.
pub const SLOT_WAIT: Duration = Duration::from_secs(3);

/// How fast a request body must arrive. A body being received holds one of the
/// [`UPLOAD_SLOTS`], so a client that opens a request and then stalls, or sends a byte
/// now and then, would otherwise keep other artists out for the whole deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPace {
    /// The whole body must arrive within this.
    pub deadline: Duration,
    /// Time allowed before the rate counts (connection set-up, a slow first packet).
    pub grace: Duration,
    /// After `grace`, the body must keep up an average of this many bytes a second: at
    /// time `t` at least `(t - grace) * min_bytes_per_sec` bytes have arrived. A stalled
    /// client is cut off at about `grace` plus what it sent divided by the rate.
    pub min_bytes_per_sec: u64,
}

impl Default for BodyPace {
    /// 30 s for the whole body (a 4 MB upload needs about 1 Mbit/s); after 5 s, at
    /// least 16 KiB a second, which a phone on a poor connection clears easily.
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(30),
            grace: Duration::from_secs(5),
            min_bytes_per_sec: 16 * 1024,
        }
    }
}

impl BodyPace {
    /// The latest time the next chunk may arrive, given `received` bytes so far.
    fn next_chunk_by(&self, start: tokio::time::Instant, received: u64) -> tokio::time::Instant {
        let earned =
            Duration::from_secs_f64(received as f64 / self.min_bytes_per_sec.max(1) as f64);
        (start + self.grace + earned).min(start + self.deadline)
    }
}

/// A body that is too slow: see [`BodyPace`].
#[derive(Debug, thiserror::Error)]
#[error("the request body arrived too slowly")]
struct TooSlow;

/// `body`, cut off with an error (and `too_slow` set) once it falls behind `pace`.
fn paced(body: Body, pace: BodyPace, too_slow: Arc<AtomicBool>) -> Body {
    let start = tokio::time::Instant::now();
    let chunks = body.into_data_stream();
    let stream = futures::stream::unfold(
        (chunks, 0u64, false),
        move |(mut chunks, received, ended)| {
            let too_slow = too_slow.clone();
            async move {
                if ended {
                    return None;
                }
                let by = pace.next_chunk_by(start, received);
                match tokio::time::timeout_at(by, chunks.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        let received = received + chunk.len() as u64;
                        Some((Ok(chunk), (chunks, received, false)))
                    }
                    Ok(Some(Err(e))) => Some((Err(e), (chunks, received, true))),
                    Ok(None) => None,
                    Err(_elapsed) => {
                        too_slow.store(true, Ordering::SeqCst);
                        Some((Err(axum::Error::new(TooSlow)), (chunks, received, true)))
                    }
                }
            }
        },
    );
    Body::from_stream(stream)
}

/// Global token bucket: a burst of 20 uploads, then one a second.
pub const BUCKET_CAPACITY: f64 = 20.0;
pub const BUCKET_REFILL_PER_SEC: f64 = 1.0;

/// Processes one upload. Production uses [`Subprocess`]; route tests use
/// [`InProcess`] or fakes.
#[async_trait::async_trait]
pub trait Processor: Send + Sync {
    /// `permit` is the processing slot: hold it until the work has really stopped, not
    /// just until this returns (a timed-out thread may still be running).
    async fn process(
        &self,
        bytes: Vec<u8>,
        fixes: Vec<Fix>,
        permit: OwnedSemaphorePermit,
    ) -> Outcome;
}

/// Each upload in a fresh `arena studio-worker` child (see [`arena::studio_worker`]).
pub struct Subprocess(pub Worker);

#[async_trait::async_trait]
impl Processor for Subprocess {
    async fn process(
        &self,
        bytes: Vec<u8>,
        fixes: Vec<Fix>,
        permit: OwnedSemaphorePermit,
    ) -> Outcome {
        self.0.run_holding(&bytes, &fixes, permit).await
    }
}

/// [`design_kit::process_on_big_stack`] in this process. No crash isolation: for tests.
pub struct InProcess {
    pub limits: Limits,
    pub deadline: Duration,
}

#[async_trait::async_trait]
impl Processor for InProcess {
    async fn process(
        &self,
        bytes: Vec<u8>,
        fixes: Vec<Fix>,
        permit: OwnedSemaphorePermit,
    ) -> Outcome {
        // The permit moves into the processing thread and is released when the work
        // ends, even if we stop waiting first.
        let rx = design_kit::process_on_big_stack(bytes, &self.limits, &fixes, permit);
        match tokio::time::timeout(self.deadline, rx).await {
            Ok(Ok(result)) => Outcome::from_result(result),
            Ok(Err(_)) => Outcome::Internal(eyre!("the processing thread ended without a result")),
            Err(_elapsed) => Outcome::TimedOut,
        }
    }
}

/// A token bucket with an explicit clock, so tests can drive time.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl TokenBucket {
    /// A full bucket.
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last: None,
        }
    }

    /// Take a token at `now`, after refilling for the time since the last call.
    pub fn try_take(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            let elapsed = now.saturating_duration_since(last).as_secs_f64();
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The studio's guards and processor. One per process (built with `AppState`), so the
/// semaphores and the bucket are process-wide.
#[derive(Clone)]
pub struct StudioState {
    pub(crate) upload_slots: Arc<Semaphore>,
    pub(crate) studio_slot: Arc<Semaphore>,
    pub(crate) bucket: Arc<Mutex<TokenBucket>>,
    pub(crate) clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    pub(crate) processor: Arc<dyn Processor>,
    pub(crate) slot_wait: Duration,
    pub(crate) body_pace: BodyPace,
}

impl StudioState {
    pub fn new(processor: Arc<dyn Processor>) -> Self {
        Self {
            upload_slots: Arc::new(Semaphore::new(UPLOAD_SLOTS)),
            studio_slot: Arc::new(Semaphore::new(STUDIO_SLOTS)),
            bucket: Arc::new(Mutex::new(TokenBucket::new(
                BUCKET_CAPACITY,
                BUCKET_REFILL_PER_SEC,
            ))),
            clock: Arc::new(Instant::now),
            processor,
            slot_wait: SLOT_WAIT,
            body_pace: BodyPace::default(),
        }
    }

    /// Production: workers are this executable's `studio-worker` subcommand.
    pub fn subprocess() -> std::io::Result<Self> {
        let worker = Worker::current_exe()?;
        Ok(Self::new(Arc::new(Subprocess(worker))))
    }

    /// Processing in this process (tests: the test binary has no worker subcommand),
    /// with the worker's wall-clock deadline.
    pub fn in_process() -> Self {
        Self::new(Arc::new(InProcess {
            limits: Limits::default(),
            deadline: WorkerLimits::default().wall,
        }))
    }

    fn take_token(&self) -> bool {
        let now = (self.clock)();
        // A poisoned lock only means a panic elsewhere mid-update; the bucket is
        // still usable.
        let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
        bucket.try_take(now)
    }
}

/// The POST route with its guards: the upload-slot middleware runs first (outermost),
/// then the body limit applies when the handler reads the body.
pub fn process_route(state: AppState) -> MethodRouter<AppState> {
    post(process)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state,
            upload_slot_guard,
        ))
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorJson,
}

fn error_response(status: StatusCode, error: ErrorJson) -> Response {
    (status, Json(ErrorBody { error })).into_response()
}

fn busy() -> Response {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorJson::new(
            "busy",
            "The studio is busy right now. Try again in a few seconds.",
        ),
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

fn rate_limited() -> Response {
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        ErrorJson::new(
            "rate_limited",
            "Lots of people are using the studio right now. Try again in a few seconds.",
        ),
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    response
}

/// Holds one of [`UPLOAD_SLOTS`] for the whole request, taken before anything reads
/// the body; marks every response `no-store`.
async fn upload_slot_guard(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = match state.studio.upload_slots.clone().try_acquire_owned() {
        Ok(_permit) => next.run(request).await,
        Err(_) => {
            tracing::info!(
                event_type = "studio_processed",
                outcome = "busy",
                stage = "upload_slots"
            );
            busy()
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `fix=flip&fix=fit` (or `fix=flip,fit`). `Err` names the first unknown value.
fn parse_fixes(query: Option<&str>) -> Result<Vec<Fix>, String> {
    let mut fixes = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        if key != "fix" {
            continue;
        }
        for name in value.split(',').filter(|n| !n.is_empty()) {
            let fix = Fix::parse(name).ok_or_else(|| name.to_string())?;
            if !fixes.contains(&fix) {
                fixes.push(fix);
            }
        }
    }
    Ok(fixes)
}

/// POST /customizations/studio/process
pub async fn process(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    request: Request,
) -> ServerResult<Response, StatusCode> {
    let started = Instant::now();
    let studio = &state.studio;
    let fixes = match parse_fixes(query.as_deref()) {
        Ok(fixes) => fixes,
        Err(_unknown) => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                ErrorJson::new("bad_fix", "Unknown fix. Use fix=flip or fix=fit."),
            ));
        }
    };

    if !studio.take_token() {
        tracing::info!(event_type = "studio_processed", outcome = "rate_limited");
        return Ok(rate_limited());
    }

    let too_slow = Arc::new(AtomicBool::new(false));
    let pace = studio.body_pace;
    let request = request.map(|body| paced(body, pace, too_slow.clone()));
    let read = tokio::time::timeout(pace.deadline, Bytes::from_request(request, &state)).await;
    let upload_timeout = || {
        tracing::info!(event_type = "studio_processed", outcome = "upload_timeout");
        error_response(
            StatusCode::REQUEST_TIMEOUT,
            ErrorJson::new(
                "upload_timeout",
                "The upload was too slow. Check your connection and try again.",
            ),
        )
    };
    let bytes = match read {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(rejection)) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            let too_large = ProcessError::TooLarge {
                bytes: MAX_BODY_BYTES + 1,
                max: MAX_BODY_BYTES,
            };
            return Ok(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorJson::from(&too_large),
            ));
        }
        Ok(Err(_)) if too_slow.load(Ordering::SeqCst) => return Ok(upload_timeout()),
        Ok(Err(rejection)) => {
            return Ok(error_response(
                rejection.status(),
                ErrorJson::new("bad_request", "We couldn't receive that file. Try again."),
            ));
        }
        Err(_elapsed) => return Ok(upload_timeout()),
    };

    let permit =
        match tokio::time::timeout(studio.slot_wait, studio.studio_slot.clone().acquire_owned())
            .await
        {
            Ok(permit) => permit.wrap_err("the studio processing semaphore was closed")?,
            Err(_elapsed) => {
                tracing::info!(
                    event_type = "studio_processed",
                    outcome = "busy",
                    stage = "studio_slot"
                );
                return Ok(busy());
            }
        };

    let len = bytes.len();
    let outcome = studio
        .processor
        .process(Vec::from(bytes), fixes.clone(), permit)
        .await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let fixes = fixes
        .iter()
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join(",");

    match outcome {
        Outcome::Shape(shape) => {
            let codes = |lints: &[arena::studio_worker::LintJson]| {
                lints
                    .iter()
                    .map(|l| l.code.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            tracing::info!(
                event_type = "studio_processed",
                outcome = "ok",
                input = shape.input.as_str(),
                strategy = shape.strategy.as_str(),
                head_lints = %codes(&shape.lints.head),
                tail_lints = %codes(&shape.lints.tail),
                info_lints = %codes(&shape.info),
                fixes = %fixes,
                bytes = len,
                duration_ms,
            );
            Ok(Json(*shape).into_response())
        }
        Outcome::Rejected(error) => {
            tracing::info!(
                event_type = "studio_processed",
                outcome = "rejected",
                error_code = %error.code,
                fixes = %fixes,
                bytes = len,
                duration_ms,
            );
            Ok(error_response(StatusCode::UNPROCESSABLE_ENTITY, error))
        }
        Outcome::Crashed { status, stderr } => {
            tracing::warn!(
                event_type = "studio_processed",
                outcome = "crashed",
                exit_status = %status,
                stderr = %stderr,
                fixes = %fixes,
                bytes = len,
                duration_ms,
                "studio worker died; answering too_complex"
            );
            Ok(error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorJson::too_complex(),
            ))
        }
        Outcome::TimedOut => {
            tracing::warn!(
                event_type = "studio_processed",
                outcome = "timed_out",
                fixes = %fixes,
                bytes = len,
                duration_ms,
                "studio processing missed its deadline; answering busy"
            );
            Ok(busy())
        }
        Outcome::Internal(report) => Err(report.wrap_err("studio processing failed").into()),
    }
}
