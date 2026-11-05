//! Achievement earn clips: a short board-only GIF around the turn that
//! earned an achievement — swept from live earns, rendered in the
//! background from the game's own frames, stored in the game-backups
//! bucket, and served as stored bytes by the share page.

use color_eyre::eyre::{Context as _, bail, eyre};
use uuid::Uuid;

use cja::jobs::Job as _;

use crate::archive::LoadArchiveError;
use crate::engine_models::EngineGameFrame;
use crate::models::game::GameRow;
use crate::models::turn::get_turn_frames_range;
use crate::og::board::BoardArt;
use crate::routes::game::api::archive_row_kind;
use crate::state::AppState;

/// Turns of game before the earning turn a clip covers.
pub(crate) const CLIP_TURNS_BEFORE: i32 = 20;
/// Turns of game after the earning turn a clip covers.
pub(crate) const CLIP_TURNS_AFTER: i32 = 5;
/// Clip frame size in pixels (square).
pub(crate) const CLIP_SIDE: u16 = 504;
/// Per-frame GIF delay in centiseconds (200 ms).
pub(crate) const CLIP_FRAME_DELAY_CS: u16 = 20;
/// NeuQuant speed for the GIF encoder: 1 slowest/best .. 30 fastest.
const CLIP_QUANT_SPEED: i32 = 10;
/// Render attempts before a clip is terminally failed.
pub(crate) const CLIP_MAX_ATTEMPTS: i32 = 3;
/// Render jobs enqueue below game runners and above backups/prunes.
pub(crate) const CLIP_JOB_PRIORITY: i32 = -5;
/// Attempt lease: longer than any render + slot wait (≤ ~30 s), shorter
/// than the sweep's stale window (300 s), aligned with cja's reclaim
/// window (240 s) so a reclaimed re-run can always re-claim.
pub(crate) const CLIP_ATTEMPT_LEASE_SECS: f64 = 240.0;
/// A pending row whose render job went missing is re-driven after this long.
pub(crate) const CLIP_STALE_AFTER_SECS: f64 = 300.0;
/// New clip rows per sweep tick (bounds render arrivals; see the
/// starvation arithmetic in the PR description).
pub(crate) const SWEEP_BATCH: i64 = 5;
/// The sweep inserts nothing new while this many rows are pending
/// (bounds workers parked on the render slot).
pub(crate) const CLIP_MAX_PENDING: i64 = 10;

/// One clip renders at a time per process: the shared instance runs
/// games, backups and social cards on one vCPU, so clip painting and GIF
/// encoding run on the blocking pool under this single permit. Never
/// shared with `backup.rs`'s `BACKUP_SLOT` or `routes/og.rs`'s
/// `RENDER_PERMITS`.
static CLIP_RENDER_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The stored-object key for one earn's clip. Earn-keyed, never
/// game-keyed, so objects (and their URLs) carry no game id.
pub(crate) fn clip_object_path(earn_id: Uuid) -> String {
    format!("clips/achievements/{earn_id}.gif")
}

/// The inclusive turn window a clip covers, clamped to the game's bounds
/// (turns are 0-based; `last_turn` is the last renderable turn). A NULL
/// `turn_number` falls back to the game's last turn.
pub(crate) fn clip_window(earning_turn: Option<i32>, last_turn: i32) -> (i32, i32) {
    let last = last_turn.max(0);
    let t = earning_turn.unwrap_or(last).clamp(0, last);
    let from = (t - CLIP_TURNS_BEFORE).max(0);
    let to = (t + CLIP_TURNS_AFTER).min(last);
    (from, to.max(from))
}

/// One painted frame as straight-alpha RGBA bytes.
fn frame_rgba(art: &BoardArt, side: u16) -> cja::Result<Vec<u8>> {
    let mut canvas = crate::og::canvas::Canvas::with_dimensions(u32::from(side), u32::from(side))?;
    canvas.board(art, 0.0, 0.0, f32::from(side));
    Ok(canvas.into_rgba())
}

/// Paint each art to RGBA and encode the frames as one looping GIF. One
/// RGBA buffer is live at a time (each frame's buffer drops at the end of
/// its iteration); quantization runs in place via
/// [`gif::Frame::from_rgba_speed`].
fn encode_clip_gif(arts: &[BoardArt], side: u16, delay_cs: u16) -> cja::Result<Vec<u8>> {
    if arts.is_empty() {
        bail!("Cannot encode a clip with no frames");
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut bytes, side, side, &[])
            .wrap_err("Failed to create clip GIF encoder")?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .wrap_err("Failed to set clip GIF repeat")?;
        for art in arts {
            let mut rgba = frame_rgba(art, side)?;
            let mut frame = gif::Frame::from_rgba_speed(side, side, &mut rgba, CLIP_QUANT_SPEED);
            frame.delay = delay_cs;
            encoder
                .write_frame(&frame)
                .wrap_err("Failed to write clip GIF frame")?;
        }
    }
    Ok(bytes)
}

/// The sweep: terminally fail stranded rows, re-drive stale pending
/// rows, insert clip rows for new public live earns (bounded by
/// SWEEP_BATCH and CLIP_MAX_PENDING) and enqueue their renders.
/// Idempotent. Called every ACHIEVEMENT_CLIPS_SWEEP_INTERVAL_SECS.
pub(crate) async fn run_clip_sweep(state: &AppState) -> cja::Result<()> {
    // Stranded rows: a process death during the final attempt leaves
    // `pending, attempts >= cap`, which neither the claim nor the stale
    // re-drive will touch. Terminal-fail them and say so in Eyes.
    let stranded = sqlx::query_scalar!(
        r#"UPDATE achievement_clips
           SET status = 'failed', updated_at = NOW()
           WHERE status = 'pending' AND attempts >= $1
             AND updated_at < NOW() - make_interval(secs => $2)
           RETURNING achievement_earn_id"#,
        CLIP_MAX_ATTEMPTS,
        CLIP_STALE_AFTER_SECS,
    )
    .fetch_all(&state.db)
    .await
    .wrap_err("Failed to terminally fail stranded achievement clips")?;
    for earn_id in &stranded {
        tracing::info!(
            event_type = "clip_rendered",
            result = "failed",
            ms = 0,
            bytes = 0,
            achievement_earn_id = %earn_id,
            "Achievement clip failed: attempts exhausted"
        );
    }

    // Stale pending rows: the render job went missing (lost enqueue,
    // dead worker past reclaim). The updated_at bump means exactly one
    // re-enqueue per stale window.
    let stale = sqlx::query_scalar!(
        r#"UPDATE achievement_clips
           SET updated_at = NOW()
           WHERE status = 'pending' AND attempts < $1
             AND updated_at < NOW() - make_interval(secs => $2)
           RETURNING achievement_earn_id"#,
        CLIP_MAX_ATTEMPTS,
        CLIP_STALE_AFTER_SECS,
    )
    .fetch_all(&state.db)
    .await
    .wrap_err("Failed to re-drive stale achievement clips")?;
    for earn_id in stale {
        enqueue_clip_render(state, earn_id).await?;
    }

    // New earns: live, with a game, on no hiding route, not yet tracked.
    let pending: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM achievement_clips WHERE status = 'pending'"#
    )
    .fetch_one(&state.db)
    .await
    .wrap_err("Failed to count pending achievement clips")?;
    let room = CLIP_MAX_PENDING - pending;
    if room <= 0 {
        tracing::debug!(
            pending,
            "Achievement clip sweep: pending cap reached, no new clips"
        );
        return Ok(());
    }
    let candidates = sqlx::query_scalar!(
        r#"SELECT ae.achievement_earn_id
           FROM achievement_earns ae
           WHERE ae.live AND ae.game_id IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM achievement_clips ac
                             WHERE ac.achievement_earn_id = ae.achievement_earn_id)
             AND NOT EXISTS (SELECT 1 FROM leaderboard_games lg
                             JOIN leaderboards l ON l.leaderboard_id = lg.leaderboard_id
                             WHERE lg.game_id = ae.game_id AND l.visibility = 'private')
             AND NOT EXISTS (SELECT 1 FROM game_battlesnakes gb
                             JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
                             JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
                             WHERE gb.game_id = ae.game_id AND l.visibility = 'private')
             AND NOT EXISTS (SELECT 1 FROM match_games mg
                             JOIN tournament_matches tm ON tm.match_id = mg.match_id
                             JOIN tournaments t ON t.tournament_id = tm.tournament_id
                             WHERE mg.game_id = ae.game_id AND t.visibility <> 'public')
           ORDER BY ae.earned_at DESC
           LIMIT $1"#,
        room.min(SWEEP_BATCH),
    )
    .fetch_all(&state.db)
    .await
    .wrap_err("Failed to find achievement earns needing clips")?;
    for earn_id in candidates {
        let inserted = sqlx::query_scalar!(
            r#"INSERT INTO achievement_clips (achievement_earn_id, status)
               VALUES ($1, 'pending')
               ON CONFLICT (achievement_earn_id) DO NOTHING
               RETURNING achievement_earn_id"#,
            earn_id,
        )
        .fetch_optional(&state.db)
        .await
        .wrap_err("Failed to insert achievement clip row")?;
        if inserted.is_some() {
            enqueue_clip_render(state, earn_id).await?;
        }
    }
    Ok(())
}

/// Enqueue one render job for one earn at CLIP_JOB_PRIORITY.
async fn enqueue_clip_render(state: &AppState, earn_id: Uuid) -> cja::Result<()> {
    crate::jobs::RenderAchievementClipJob {
        achievement_earn_id: earn_id,
    }
    .enqueue(
        state.clone(),
        format!("render achievement clip {earn_id}"),
        Some(CLIP_JOB_PRIORITY),
    )
    .await
    .wrap_err_with(|| format!("Failed to enqueue render for achievement clip {earn_id}"))
}

/// One render attempt for one earn's clip. The claim is the only
/// concurrency control. Render failures are recorded and swallowed (the
/// sweep is the retry driver); only bookkeeping failures propagate to cja.
pub(crate) async fn render_achievement_clip(state: &AppState, earn_id: Uuid) -> cja::Result<()> {
    let earn = sqlx::query!(
        "SELECT game_id, turn_number FROM achievement_earns WHERE achievement_earn_id = $1",
        earn_id,
    )
    .fetch_optional(&state.db)
    .await
    .wrap_err("Failed to read achievement earn for clip")?;
    let Some(earn) = earn else {
        return Ok(());
    };
    // A game deleted after the sweep (or a backfilled earn that snuck in):
    // nothing to render, terminally fail the row if there is one.
    let Some(game_id) = earn.game_id else {
        return fail_clip_for_missing_game(state, earn_id).await;
    };
    let Some(record) = crate::models::game::get_game_row_by_any_id(&state.db, game_id)
        .await
        .wrap_err("Failed to read clip game row")?
    else {
        return fail_clip_for_missing_game(state, earn_id).await;
    };
    // Privacy re-check at render time: a game that flipped private since
    // the sweep gets no render, no object, and no clip row. Flipping back
    // public re-creates it through the next sweep.
    if crate::routes::achievements::game_is_hidden(&state.db, game_id).await? {
        let deleted = sqlx::query_scalar!(
            "DELETE FROM achievement_clips WHERE achievement_earn_id = $1 RETURNING achievement_earn_id",
            earn_id,
        )
        .fetch_optional(&state.db)
        .await
        .wrap_err("Failed to delete clip row for a hidden game")?;
        if deleted.is_some() {
            tracing::info!(
                achievement_earn_id = %earn_id,
                "Deleted achievement clip row: game is not public"
            );
        }
        return Ok(());
    }
    // Claim the attempt: one atomic UPDATE is the only concurrency
    // control, so duplicate jobs (cja's enqueue has no dedup) and
    // concurrent runs mostly no-op.
    let claimed_attempts = sqlx::query_scalar!(
        r#"UPDATE achievement_clips
           SET attempts = attempts + 1, last_attempt_at = NOW(), updated_at = NOW()
           WHERE achievement_earn_id = $1
             AND status = 'pending'
             AND attempts < $2
             AND (last_attempt_at IS NULL
                  OR last_attempt_at < NOW() - make_interval(secs => $3))
           RETURNING attempts AS "attempts!""#,
        earn_id,
        CLIP_MAX_ATTEMPTS,
        CLIP_ATTEMPT_LEASE_SECS,
    )
    .fetch_optional(&state.db)
    .await
    .wrap_err("Failed to claim achievement clip attempt")?;
    let Some(attempts) = claimed_attempts else {
        return Ok(());
    };
    match render_and_store(state, earn_id, &record, earn.turn_number).await {
        Ok((bytes, ms)) => {
            sqlx::query!(
                "UPDATE achievement_clips
                 SET status = 'rendered', byte_size = $2, render_ms = $3,
                     rendered_at = NOW(), updated_at = NOW()
                 WHERE achievement_earn_id = $1 AND status = 'pending'",
                earn_id,
                i32::try_from(bytes).unwrap_or(i32::MAX),
                ms,
            )
            .execute(&state.db)
            .await
            .wrap_err("Failed to record rendered achievement clip")?;
            tracing::info!(
                event_type = "clip_rendered",
                result = "rendered",
                ms,
                bytes,
                achievement_earn_id = %earn_id,
                "Achievement clip rendered"
            );
        }
        Err(report) => {
            // The attempt is counted and the failure recorded; the job
            // returns Ok either way — a clip failure never dead-letters
            // or blocks anything. The sweep is the retry driver.
            tracing::warn!(
                error = %format!("{report:#}"),
                achievement_earn_id = %earn_id,
                "Achievement clip render failed"
            );
            if attempts >= CLIP_MAX_ATTEMPTS {
                sqlx::query!(
                    "UPDATE achievement_clips
                     SET status = 'failed', updated_at = NOW()
                     WHERE achievement_earn_id = $1 AND status = 'pending'",
                    earn_id,
                )
                .execute(&state.db)
                .await
                .wrap_err("Failed to terminally fail achievement clip")?;
            }
            tracing::info!(
                event_type = "clip_rendered",
                result = "failed",
                ms = 0,
                bytes = 0,
                achievement_earn_id = %earn_id,
                "Achievement clip render failed"
            );
        }
    }
    Ok(())
}

/// Terminal cleanup when the earn's game is gone: fail a pending row (if
/// any) and emit the Eyes event for the transition.
async fn fail_clip_for_missing_game(state: &AppState, earn_id: Uuid) -> cja::Result<()> {
    let failed = sqlx::query_scalar!(
        r#"UPDATE achievement_clips
           SET status = 'failed', updated_at = NOW()
           WHERE achievement_earn_id = $1 AND status = 'pending'
           RETURNING achievement_earn_id"#,
        earn_id,
    )
    .fetch_optional(&state.db)
    .await
    .wrap_err("Failed to fail achievement clip for a deleted game")?;
    if failed.is_some() {
        tracing::info!(
            event_type = "clip_rendered",
            result = "failed",
            ms = 0,
            bytes = 0,
            achievement_earn_id = %earn_id,
            "Achievement clip failed: game is gone"
        );
    }
    Ok(())
}

/// The clip window's frames from whichever source serves this game:
/// DB turns for recent games, the archive for pruned games. Parse
/// failure of any frame is a render failure, never a dropped frame.
async fn load_clip_frames(
    state: &AppState,
    record: &GameRow,
    earning_turn: Option<i32>,
) -> cja::Result<Vec<EngineGameFrame>> {
    let game_id = record.game.game_id;
    if crate::routes::game::api::serves_from_archive(&state.db, record).await? {
        let path = record.gcs_path.clone().unwrap_or_default();
        let archived = match state
            .archive
            .load(game_id, &path, archive_row_kind(record))
            .await
        {
            Ok(archived) => archived,
            Err(LoadArchiveError::ObjectMissing { path }) => {
                return Err(eyre!("Archive object missing at {path}"));
            }
            Err(LoadArchiveError::Failed(report)) => return Err(report),
        };
        return state.archive.frames_for_clip(archived, earning_turn).await;
    }
    // Same filter as get_turn_frames_range, so the window can't end on a
    // turn with no frame.
    let last_turn = sqlx::query_scalar!(
        r#"SELECT MAX(turn_number) FROM turns WHERE game_id = $1 AND frame_data IS NOT NULL"#,
        game_id,
    )
    .fetch_one(&state.db)
    .await
    .wrap_err("Failed to read a game's last clip turn")?;
    let Some(last_turn) = last_turn else {
        return Ok(Vec::new());
    };
    let (from, to) = clip_window(earning_turn, last_turn);
    get_turn_frames_range(&state.db, game_id, from, to)
        .await?
        .into_iter()
        .map(|frame| serde_json::from_value(frame).wrap_err("Failed to parse clip frame"))
        .collect()
}

/// Paint + encode + store. Returns (bytes, render_ms), where render_ms
/// measures encode + object put only (the clock starts at the render
/// slot, not at job start).
async fn render_and_store(
    state: &AppState,
    earn_id: Uuid,
    record: &GameRow,
    earning_turn: Option<i32>,
) -> cja::Result<(usize, i32)> {
    if !state.object_store.is_configured() {
        bail!("Object store not configured: cannot store achievement clip");
    }
    let frames = load_clip_frames(state, record, earning_turn).await?;
    if frames.is_empty() {
        bail!("No frames in the achievement clip window");
    }
    // One clip at a time per process; the permit moves into the blocking
    // closure so no cancellation path frees the slot mid-render.
    let permit = CLIP_RENDER_SLOT
        .acquire()
        .await
        .wrap_err("Clip render slot closed")?;
    #[cfg(test)]
    clip_seam_handoff(earn_id)?;
    let started = std::time::Instant::now();
    let (width, height) = crate::routes::og::board_sides(&record.game.board_size);
    let rendered = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        #[cfg(test)]
        clip_seam_in_render(earn_id);
        let arts: Vec<BoardArt> = frames
            .iter()
            .map(|frame| crate::routes::og::board(width, height, Some(frame), true))
            .collect();
        encode_clip_gif(&arts, CLIP_SIDE, CLIP_FRAME_DELAY_CS)
    })
    .await;
    let bytes = match rendered {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(report)) => return Err(report),
        Err(join_error) => {
            // A painter panic is contained by spawn_blocking: it is a
            // normal render failure, not a process failure.
            return Err(eyre!("Clip render task panicked: {join_error}"));
        }
    };
    let byte_count = bytes.len();
    state
        .object_store
        .put(&clip_object_path(earn_id), bytes)
        .await?;
    Ok((byte_count, started.elapsed().as_millis() as i32))
}

/// Test seams (#[cfg(test)] only, keyed by earn id: #[sqlx::test]s run in
/// parallel inside one process, so process-global booleans would bleed
/// between tests).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ClipSeam {
    /// Earns whose next render attempt panics at handoff (view-time proof).
    pub(crate) poisoned: std::collections::HashSet<Uuid>,
    /// Earns whose next render panics inside the blocking closure
    /// (JoinError containment).
    pub(crate) panic_in_render: std::collections::HashSet<Uuid>,
    /// Earns whose renders fail without panicking (attempt-cap test).
    pub(crate) fail_render: std::collections::HashSet<Uuid>,
    /// Render handoffs per earn (the control counter).
    pub(crate) calls: std::collections::HashMap<Uuid, usize>,
}

#[cfg(test)]
pub(crate) static CLIP_SEAM: std::sync::LazyLock<std::sync::Mutex<ClipSeam>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(ClipSeam::default()));

/// Runs in `render_and_store` right after the render permit: counts the
/// handoff, then optionally panics or fails, for tests.
#[cfg(test)]
fn clip_seam_handoff(earn_id: Uuid) -> cja::Result<()> {
    let mut seam = CLIP_SEAM.lock().unwrap_or_else(|e| e.into_inner());
    *seam.calls.entry(earn_id).or_insert(0) += 1;
    if seam.poisoned.contains(&earn_id) {
        panic!("poisoned clip render");
    }
    if seam.fail_render.contains(&earn_id) {
        bail!("injected clip render failure");
    }
    Ok(())
}

/// First statement of the blocking render closure: panics for tests.
#[cfg(test)]
fn clip_seam_in_render(earn_id: Uuid) {
    let seam = CLIP_SEAM.lock().unwrap_or_else(|e| e.into_inner());
    if seam.panic_in_render.contains(&earn_id) {
        panic!("injected panic inside clip render");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_window_clamps_to_game_bounds() {
        assert_eq!(clip_window(Some(30), 30), (10, 30));
        // The 26-frame max: 20 before + 5 after + the earning turn.
        assert_eq!(clip_window(Some(25), 40), (5, 30));
        // Early turn: clamped at zero.
        assert_eq!(clip_window(Some(2), 30), (0, 7));
        // NULL turn_number: the whole tail window from the last turn.
        assert_eq!(clip_window(None, 12), (0, 12));
        // A stale turn_number past the game's bounds clamps into them.
        assert_eq!(clip_window(Some(9), 3), (0, 3));
        assert_eq!(clip_window(Some(-4), 3), (0, 3));
        // A frameless game still yields a sane (empty-render) window.
        assert_eq!(clip_window(Some(5), -1), (0, 0));
    }

    #[test]
    fn board_sides_clamps_custom_sizes() {
        use crate::models::game::GameBoardSize;
        use crate::routes::og::board_sides;
        assert_eq!(
            board_sides(&GameBoardSize::Custom("5000x5000".to_string())),
            (25, 25)
        );
        assert_eq!(
            board_sides(&GameBoardSize::Custom("0x0".to_string())),
            (1, 1)
        );
        assert_eq!(
            board_sides(&GameBoardSize::Custom("axb".to_string())),
            (11, 11)
        );
        assert_eq!(board_sides(&GameBoardSize::Small), (7, 7));
        assert_eq!(board_sides(&GameBoardSize::Medium), (11, 11));
        assert_eq!(board_sides(&GameBoardSize::Large), (19, 19));
    }

    /// A one-snake board art whose body grows by turn: distinct frames.
    fn art(turn: i32) -> BoardArt {
        BoardArt {
            width: 11,
            height: 11,
            snakes: vec![crate::og::board::BoardSnake {
                body: (0..=turn).map(|i| (5, 5 + i)).collect(),
                color: crate::og::canvas::snake_color("#ff3d8a"),
                head: None,
                tail: None,
                eliminated: false,
            }],
            food: vec![(1, 1)],
            hazards: vec![],
        }
    }

    /// Decode a GIF to RGBA frames via the gif crate's own reader.
    fn decode_gif(bytes: &[u8]) -> (u16, u16, Vec<(u16, Vec<u8>)>) {
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options
            .read_info(std::io::Cursor::new(bytes))
            .expect("decodable gif");
        let (width, height) = (decoder.width(), decoder.height());
        let mut frames = Vec::new();
        while let Some(frame) = decoder.read_next_frame().expect("frame pixels") {
            frames.push((frame.delay, frame.buffer.to_vec()));
        }
        (width, height, frames)
    }

    #[test]
    fn encoded_gif_decodes_with_expected_frames() {
        let arts: Vec<BoardArt> = (0..4).map(art).collect();
        let bytes = encode_clip_gif(&arts, CLIP_SIDE, CLIP_FRAME_DELAY_CS).unwrap();
        let (width, height, frames) = decode_gif(&bytes);
        assert_eq!((width, height), (CLIP_SIDE, CLIP_SIDE));
        assert_eq!(frames.len(), 4);
        for (delay, _) in &frames {
            assert_eq!(*delay, CLIP_FRAME_DELAY_CS);
        }
        let first = &frames[0].1;
        let last = &frames[3].1;
        assert_ne!(first, last, "distinct boards encode distinct frames");
        // Control: the same board twice encodes identical first frames.
        let same = encode_clip_gif(&[art(2), art(2)], CLIP_SIDE, CLIP_FRAME_DELAY_CS).unwrap();
        let (_, _, same_frames) = decode_gif(&same);
        assert_eq!(same_frames[0].1, same_frames[1].1);
    }

    #[test]
    fn empty_clip_fails_to_encode() {
        assert!(encode_clip_gif(&[], CLIP_SIDE, CLIP_FRAME_DELAY_CS).is_err());
    }

    // ---- integration helpers ----

    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    use crate::object_store::InMemoryObjectStore;
    use crate::object_store::ObjectStore as _;

    const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

    /// Test state whose archive reader and object store are the SAME
    /// in-memory store, so archive tests can seed objects and every test
    /// can count puts.
    async fn clip_state(pool: &PgPool) -> (AppState, std::sync::Arc<InMemoryObjectStore>) {
        let store = std::sync::Arc::new(InMemoryObjectStore::default());
        let mut state = AppState::test_from_pool(pool.clone());
        state.object_store = store.clone();
        state.archive = crate::archive::ArchiveReader::new(store.clone(), 0);
        (state, store)
    }

    async fn user(pool: &PgPool, github_id: i64) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES ($1, $2, '') RETURNING user_id",
        )
        .bind(github_id)
        .bind(format!("clip-{github_id}"))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn snake(pool: &PgPool, owner: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
        )
        .bind(owner)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn game(pool: &PgPool, status: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO games (board_size, game_type, status, finished_at) VALUES ('11x11', 'Standard', $1, CASE WHEN $1 = 'finished' THEN NOW() ELSE NULL END) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn participant(pool: &PgPool, game_id: Uuid, snake_id: Uuid, placement: i32) {
        sqlx::query(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement) VALUES ($1, $2, $3)",
        )
        .bind(game_id)
        .bind(snake_id)
        .bind(placement)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A live earn with full context; returns its id.
    async fn seed_live_earn(
        pool: &PgPool,
        user_id: Uuid,
        slug: &str,
        snake_id: Option<Uuid>,
        game_id: Option<Uuid>,
        turn_number: Option<i32>,
    ) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO achievement_earns (user_id, achievement, battlesnake_id, game_id, turn_number, live, earned_at)
             VALUES ($1, $2, $3, $4, $5, TRUE, TIMESTAMPTZ '2026-11-05 04:00:00+00')
             RETURNING achievement_earn_id",
        )
        .bind(user_id)
        .bind(slug)
        .bind(snake_id)
        .bind(game_id)
        .bind(turn_number)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A PascalCase engine frame whose body and food differ every turn, so
    /// decoded frames are distinguishable per turn.
    fn turn_frame(turn: i32) -> serde_json::Value {
        let body: Vec<serde_json::Value> = (0..=(turn.rem_euclid(20)))
            .map(|i| serde_json::json!({"X": 5, "Y": 5 - i}))
            .collect();
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": "clip-snake",
                "Name": "Clip Snake",
                "URL": "",
                "Body": body,
                "Health": 100,
                "Death": null,
                "Color": "#ff3d8a",
                "HeadType": "dragon",
                "TailType": "bolt",
            }],
            "Food": [{"X": (turn * 3).rem_euclid(11), "Y": (turn * 7).rem_euclid(11)}],
            "Hazards": [],
        })
    }

    async fn seed_turn(pool: &PgPool, game_id: Uuid, turn: i32) {
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(turn)
            .bind(turn_frame(turn))
            .execute(pool)
            .await
            .unwrap();
    }

    struct ClipRow {
        status: String,
        attempts: i32,
        byte_size: Option<i32>,
        render_ms: Option<i32>,
    }

    async fn clip(pool: &PgPool, earn_id: Uuid) -> Option<ClipRow> {
        sqlx::query_as!(
            ClipRow,
            r#"SELECT status, attempts, byte_size, render_ms
               FROM achievement_clips WHERE achievement_earn_id = $1"#,
            earn_id
        )
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// (name, priority) of every enqueued job for this earn.
    async fn clip_jobs(pool: &PgPool, earn_id: Uuid) -> Vec<(String, i32)> {
        sqlx::query_as("SELECT name, priority FROM jobs WHERE payload->>'achievement_earn_id' = $1")
            .bind(earn_id.to_string())
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// `clip_rendered` events for one earn (the buffer is process-global;
    /// ids are unique per test).
    fn clip_events(earn_id: Uuid) -> Vec<std::collections::BTreeMap<String, String>> {
        crate::test_capture::Capture::shared().events_with_field(
            "clip_rendered",
            "achievement_earn_id",
            &earn_id.to_string(),
        )
    }

    /// Render handoffs recorded for one earn (the control counter).
    fn calls(earn_id: Uuid) -> usize {
        CLIP_SEAM
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .calls
            .get(&earn_id)
            .copied()
            .unwrap_or(0)
    }

    fn seam<T>(f: impl FnOnce(&mut ClipSeam) -> T) -> T {
        let mut guard = CLIP_SEAM.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// Backdate `updated_at` on one clip row, working around the trigger
    /// that would overwrite it: disable, set, assert the backdate took,
    /// re-enable.
    async fn backdate_updated_at(pool: &PgPool, earn_id: Uuid) {
        sqlx::query(
            "ALTER TABLE achievement_clips DISABLE TRIGGER update_achievement_clips_updated_at",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE achievement_clips SET updated_at = NOW() - INTERVAL '10 minutes' WHERE achievement_earn_id = $1",
        )
        .bind(earn_id)
        .execute(pool)
        .await
        .unwrap();
        let updated: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT updated_at FROM achievement_clips WHERE achievement_earn_id = $1",
        )
        .bind(earn_id)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(
            updated < chrono::Utc::now() - chrono::Duration::minutes(5),
            "backdate must take (trigger disabled)"
        );
        sqlx::query(
            "ALTER TABLE achievement_clips ENABLE TRIGGER update_achievement_clips_updated_at",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// A public live earn with DB turns, swept and rendered; returns the
    /// earn id. The one-stop setup for serving/page/view-time tests.
    async fn rendered_public_earn(
        pool: &PgPool,
        state: &AppState,
        github_id: i64,
        turns: std::ops::RangeInclusive<i32>,
        turn_number: i32,
    ) -> Uuid {
        let owner = user(pool, github_id).await;
        let owner_snake = snake(pool, owner, "clip owner").await;
        let game_id = game(pool, "finished").await;
        participant(pool, game_id, owner_snake, 1).await;
        for turn in turns.clone() {
            seed_turn(pool, game_id, turn).await;
        }
        let earn = seed_live_earn(
            pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(turn_number),
        )
        .await;
        run_clip_sweep(state).await.unwrap();
        render_achievement_clip(state, earn).await.unwrap();
        assert_eq!(clip(pool, earn).await.unwrap().status, "rendered");
        earn
    }

    struct Reply {
        status: axum::http::StatusCode,
        content_type: Option<String>,
        cache_control: Option<String>,
        body: Vec<u8>,
    }

    async fn request(
        state: &AppState,
        method: &str,
        uri: &str,
        headers: Vec<(&'static str, String)>,
        form_body: Option<&str>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in &headers {
            builder = builder.header(*name, value);
        }
        let request = match form_body {
            Some(body) => builder
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router()
            .oneshot(request)
            .await
            .unwrap();
        let header = |name: axum::http::HeaderName| {
            response
                .headers()
                .get(name)
                .map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_string())
        };
        let (content_type, cache_control) = (
            header(axum::http::header::CONTENT_TYPE),
            header(axum::http::header::CACHE_CONTROL),
        );
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            content_type,
            cache_control,
            body,
        }
    }

    async fn get(state: &AppState, uri: &str) -> Reply {
        request(state, "GET", uri, vec![], None).await
    }

    async fn session_cookie(state: &AppState, pool: &PgPool, user_id: Uuid) -> String {
        let session_id: Uuid =
            sqlx::query_scalar("INSERT INTO sessions (user_id) VALUES ($1) RETURNING session_id")
                .bind(user_id)
                .fetch_one(pool)
                .await
                .unwrap();
        format!(
            "{}={}",
            crate::models::session::SESSION_COOKIE_NAME,
            crate::routes::test_support::signed_session_cookie(state, session_id)
        )
    }

    /// The user id behind an earn.
    async fn earn_owner(pool: &PgPool, earn_id: Uuid) -> Uuid {
        sqlx::query_scalar("SELECT user_id FROM achievement_earns WHERE achievement_earn_id = $1")
            .bind(earn_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // ---- render path, DB frames ----

    #[sqlx::test(migrations = "../migrations")]
    async fn renders_public_live_earn_from_db_frames(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let owner = user(&pool, 2121101).await;
        let owner_snake = snake(&pool, owner, "clip winner").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1).await;
        for turn in 0..=30 {
            seed_turn(&pool, game_id, turn).await;
        }
        let earn = seed_live_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(30),
        )
        .await;

        run_clip_sweep(&state).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempts, 0);
        // The render job lands below the default priority: a default-0
        // control enqueue in the same table makes the -5 comparable
        // (mirroring backup.rs's queue-priority control).
        crate::jobs::GameRunnerJob {
            game_id: Uuid::new_v4(),
        }
        .enqueue(state.clone(), "priority control".to_string(), None)
        .await
        .unwrap();
        assert_eq!(
            clip_jobs(&pool, earn).await,
            vec![("RenderAchievementClipJob".to_string(), CLIP_JOB_PRIORITY)]
        );
        let all: Vec<(String, i32)> = sqlx::query_as("SELECT name, priority FROM jobs")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(all.contains(&("GameRunnerJob".to_string(), 0)));

        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!(row.status, "rendered");
        let byte_size = row.byte_size.expect("rendered carries byte_size");
        assert!(byte_size > 0);
        assert!(row.render_ms.is_some());
        let bytes = store
            .get(&clip_object_path(earn))
            .await
            .unwrap()
            .expect("stored object");
        assert_eq!(bytes.len(), byte_size as usize);
        // A real GIF, decodable by the same crate.
        let (width, height, frames) = decode_gif(&bytes);
        assert_eq!((width, height), (CLIP_SIDE, CLIP_SIDE));
        assert_eq!(frames.len(), 21, "window 10..=30");

        let events = clip_events(earn);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].get("result").map(String::as_str),
            Some("rendered")
        );
        assert!(
            events[0]
                .get("bytes")
                .and_then(|b| b.parse::<i64>().ok())
                .unwrap_or(0)
                > 0
        );
        assert!(events[0].contains_key("ms"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn not_yet_archived_game_renders_from_turns(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let owner = user(&pool, 2121102).await;
        let owner_snake = snake(&pool, owner, "unarchived winner").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1).await;
        // The not-yet-archived window: turns present, no archive pointer.
        let gcs_path: Option<String> =
            sqlx::query_scalar("SELECT gcs_path FROM games WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(gcs_path.is_none());
        for turn in 0..=12 {
            seed_turn(&pool, game_id, turn).await;
        }
        let earn = seed_live_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(9),
        )
        .await;

        run_clip_sweep(&state).await.unwrap();
        render_achievement_clip(&state, earn).await.unwrap();
        assert_eq!(clip(&pool, earn).await.unwrap().status, "rendered");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn clip_range_decoded_first_and_last_turn(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;

        // Renders the stored clip for one earn seeded with turns 0..=last,
        // and decodes it to RGBA frames.
        async fn render_and_decode(
            pool: &PgPool,
            state: &AppState,
            store: &std::sync::Arc<InMemoryObjectStore>,
            github_id: i64,
            last: i32,
            turn_number: i32,
            trailing_null_row: bool,
        ) -> (Vec<Vec<u8>>, Uuid) {
            let owner = user(pool, github_id).await;
            let owner_snake = snake(pool, owner, "range snake").await;
            let game_id = game(pool, "finished").await;
            participant(pool, game_id, owner_snake, 1).await;
            for turn in 0..=last {
                seed_turn(pool, game_id, turn).await;
            }
            if trailing_null_row {
                // A rolled-back retry row with no frame: never a boundary.
                sqlx::query(
                    "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, NULL)",
                )
                .bind(game_id)
                .bind(last + 1)
                .execute(pool)
                .await
                .unwrap();
            }
            let earn = seed_live_earn(
                pool,
                owner,
                "first_win",
                Some(owner_snake),
                Some(game_id),
                Some(turn_number),
            )
            .await;
            run_clip_sweep(state).await.unwrap();
            render_achievement_clip(state, earn).await.unwrap();
            let bytes = store
                .get(&clip_object_path(earn))
                .await
                .unwrap()
                .expect("stored clip");
            let (_, _, frames) = decode_gif(&bytes);
            (frames.into_iter().map(|(_, buffer)| buffer).collect(), earn)
        }

        // The expected single-frame buffer for one turn's board.
        fn expected_frame(turn: i32) -> Vec<u8> {
            let frame: EngineGameFrame = serde_json::from_value(turn_frame(turn)).unwrap();
            let art = crate::routes::og::board(11, 11, Some(&frame), true);
            let gif = encode_clip_gif(&[art], CLIP_SIDE, CLIP_FRAME_DELAY_CS).unwrap();
            let (_, _, frames) = decode_gif(&gif);
            frames.into_iter().next().unwrap().1
        }

        // Early turn: T=2 over 31 turns → 8 frames (0..=7).
        let (frames, _earn) = render_and_decode(&pool, &state, &store, 2121111, 30, 2, false).await;
        assert_eq!(frames.len(), 8);
        assert_eq!(frames[0], expected_frame(0), "first frame is turn from");
        assert_eq!(frames[7], expected_frame(7), "last frame is turn to");
        assert_ne!(frames[0], expected_frame(1), "control: not from+1");
        assert_ne!(frames[7], expected_frame(6), "control: not to-1");

        // Mid game: T=25 over 40 turns → 26 frames (5..=30), the max.
        let (frames, _earn) =
            render_and_decode(&pool, &state, &store, 2121112, 40, 25, false).await;
        assert_eq!(frames.len(), 26);
        assert_eq!(frames[0], expected_frame(5));
        assert_eq!(frames[25], expected_frame(30));
        assert_ne!(frames[0], expected_frame(6), "control: not from+1");
        assert_ne!(frames[25], expected_frame(29), "control: not to-1");

        // Final turn (often the real case): T=40 over 41 turns → 21 frames
        // (20..=40); a trailing NULL-frame row never extends the window.
        let (frames, _earn) = render_and_decode(&pool, &state, &store, 2121113, 40, 40, true).await;
        assert_eq!(frames.len(), 21);
        assert_eq!(frames[0], expected_frame(20));
        assert_eq!(frames[20], expected_frame(40));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn attempt_cap_fails_terminally_with_failing_renderer(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121121).await;
        seam(|s| {
            s.fail_render.insert(earn);
        });

        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("pending", 1));
        sqlx::query(
            "UPDATE achievement_clips SET last_attempt_at = NOW() - INTERVAL '10 minutes' WHERE achievement_earn_id = $1",
        )
        .bind(earn)
        .execute(&pool)
        .await
        .unwrap();

        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("pending", 2));
        sqlx::query(
            "UPDATE achievement_clips SET last_attempt_at = NOW() - INTERVAL '10 minutes' WHERE achievement_earn_id = $1",
        )
        .bind(earn)
        .execute(&pool)
        .await
        .unwrap();

        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("failed", 3));
        assert!(store.get(&clip_object_path(earn)).await.unwrap().is_none());
        let events = clip_events(earn);
        assert_eq!(events.len(), 3, "one failed event per attempt");
        assert!(
            events
                .iter()
                .all(|e| e.get("result").map(String::as_str) == Some("failed"))
        );

        // Terminal: the sweep leaves it alone (no new row, job, or event).
        let jobs_before = clip_jobs(&pool, earn).await.len();
        backdate_updated_at(&pool, earn).await;
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(clip_jobs(&pool, earn).await.len(), jobs_before);
        assert_eq!(clip(&pool, earn).await.unwrap().status, "failed");
        assert_eq!(clip_events(earn).len(), 3);
    }

    /// Setup shared by failure-mode tests: a swept (pending) public earn
    /// whose frames exist, without rendering it.
    async fn rendered_public_earn_setup(pool: &PgPool, state: &AppState, github_id: i64) -> Uuid {
        let owner = user(pool, github_id).await;
        let owner_snake = snake(pool, owner, "failing clip owner").await;
        let game_id = game(pool, "finished").await;
        participant(pool, game_id, owner_snake, 1).await;
        for turn in 0..=12 {
            seed_turn(pool, game_id, turn).await;
        }
        let earn = seed_live_earn(
            pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(9),
        )
        .await;
        run_clip_sweep(state).await.unwrap();
        earn
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn transient_put_failure_then_success(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121122).await;

        store.fail_next_put();
        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("pending", 1));
        assert_eq!(clip_events(earn).len(), 1);

        sqlx::query(
            "UPDATE achievement_clips SET last_attempt_at = NOW() - INTERVAL '10 minutes' WHERE achievement_earn_id = $1",
        )
        .bind(earn)
        .execute(&pool)
        .await
        .unwrap();
        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("rendered", 2));
        assert!(store.get(&clip_object_path(earn)).await.unwrap().is_some());
        assert_eq!(clip_events(earn).len(), 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn join_error_render_failure_contained(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121123).await;
        seam(|s| {
            s.panic_in_render.insert(earn);
        });

        // A painter panic is contained by spawn_blocking: the attempt is
        // counted, the failure recorded, and the job returns Ok.
        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("pending", 1));
        assert!(store.get(&clip_object_path(earn)).await.unwrap().is_none());
        let events = clip_events(earn);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].get("result").map(String::as_str), Some("failed"));
        // The process is alive: a second attempt runs normally.
        seam(|s| {
            s.panic_in_render.remove(&earn);
        });
        sqlx::query(
            "UPDATE achievement_clips SET last_attempt_at = NOW() - INTERVAL '10 minutes' WHERE achievement_earn_id = $1",
        )
        .bind(earn)
        .execute(&pool)
        .await
        .unwrap();
        render_achievement_clip(&state, earn).await.unwrap();
        assert_eq!(clip(&pool, earn).await.unwrap().status, "rendered");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unparseable_frame_fails_render_not_dropped(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let owner = user(&pool, 2121124).await;
        let owner_snake = snake(&pool, owner, "bad frame owner").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1).await;
        for turn in 0..=12 {
            seed_turn(&pool, game_id, turn).await;
        }
        // One in-window frame is garbage: the whole render fails.
        sqlx::query("UPDATE turns SET frame_data = '{\"Turn\":\"x\"}'::jsonb WHERE game_id = $1 AND turn_number = 5")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let earn = seed_live_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(9),
        )
        .await;
        run_clip_sweep(&state).await.unwrap();

        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("pending", 1));
        assert!(store.get(&clip_object_path(earn)).await.unwrap().is_none());
        let events = clip_events(earn);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].get("result").map(String::as_str), Some("failed"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_renders_claim_once(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121125).await;

        let (first, second) = tokio::join!(
            render_achievement_clip(&state, earn),
            render_achievement_clip(&state, earn),
        );
        first.unwrap();
        second.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!(row.status, "rendered");
        assert_eq!(row.attempts, 1, "the claim admits exactly one attempt");
        assert_eq!(store.put_count(), 1);
        assert_eq!(clip_events(earn).len(), 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn duplicate_render_after_success_noops(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121126).await;

        render_achievement_clip(&state, earn).await.unwrap();
        assert_eq!(store.put_count(), 1);
        assert_eq!(calls(earn), 1);

        // A duplicate job (cja's enqueue has no dedup) re-runs and no-ops.
        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("rendered", 1));
        assert_eq!(store.put_count(), 1, "no second render");
        assert_eq!(calls(earn), 1, "no second handoff");
        assert_eq!(clip_events(earn).len(), 1);
    }

    // ---- sweep behavior ----

    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_respects_pending_cap(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;

        // A fresh live earn (candidate) whose clip row is NOT created.
        async fn candidate(pool: &PgPool, github_id: i64) -> Uuid {
            let owner = user(pool, github_id).await;
            let owner_snake = snake(pool, owner, "cap snake").await;
            let game_id = game(pool, "finished").await;
            participant(pool, game_id, owner_snake, 1).await;
            seed_turn(pool, game_id, 0).await;
            seed_live_earn(
                pool,
                owner,
                "first_win",
                Some(owner_snake),
                Some(game_id),
                Some(0),
            )
            .await
        }

        // Seven fresh candidates, no pending: one tick admits SWEEP_BATCH.
        for number in 0..7 {
            candidate(&pool, 2121131 + number).await;
        }
        run_clip_sweep(&state).await.unwrap();
        let pending = pending_count(&pool).await;
        assert_eq!(pending, SWEEP_BATCH);

        // Nine pending (5 swept + 4 hand-seeded) + three fresh candidates:
        // exactly one more fits, whichever candidate it is.
        for number in 0..4 {
            let earn = candidate(&pool, 2121141 + number).await;
            sqlx::query(
                "INSERT INTO achievement_clips (achievement_earn_id, status) VALUES ($1, 'pending')",
            )
            .bind(earn)
            .execute(&pool)
            .await
            .unwrap();
        }
        for number in 0..3 {
            candidate(&pool, 2121151 + number).await;
        }
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(pending_count(&pool).await, CLIP_MAX_PENDING);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, CLIP_MAX_PENDING, "5 swept + 4 seeded + 1 admitted");

        // At the cap: a tick inserts nothing for new candidates.
        for number in 0..3 {
            candidate(&pool, 2121161 + number).await;
        }
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(pending_count(&pool).await, CLIP_MAX_PENDING);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, CLIP_MAX_PENDING, "no new rows at the cap");
    }

    async fn pending_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips WHERE status = 'pending'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_skips_backfilled_earns(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;

        // A backfilled earn: live = false, no game by construction.
        let backfilled = user(&pool, 2121161).await;
        sqlx::query(
            "INSERT INTO achievement_earns (user_id, achievement, live, earned_at) VALUES ($1, 'first_win', FALSE, TIMESTAMPTZ '2026-10-31 12:00:00+00')",
        )
        .bind(backfilled)
        .execute(&pool)
        .await
        .unwrap();
        // A live earn whose game is gone (game_id NULL): no clip either.
        let gameless = user(&pool, 2121162).await;
        seed_live_earn(&pool, gameless, "first_win", None, None, None).await;
        // Control: a live earn with a public game gets both.
        let control = user(&pool, 2121163).await;
        let control_snake = snake(&pool, control, "backfill control").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, control_snake, 1).await;
        seed_turn(&pool, game_id, 0).await;
        let control_earn = seed_live_earn(
            &pool,
            control,
            "first_win",
            Some(control_snake),
            Some(game_id),
            Some(0),
        )
        .await;

        run_clip_sweep(&state).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        assert!(clip(&pool, control_earn).await.is_some());
        let jobs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE name = 'RenderAchievementClipJob'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 1);
        assert_eq!(store.put_count(), 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn two_sweeps_one_row_one_job(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121171).await;
        // rendered_public_earn_setup already ran one sweep.
        run_clip_sweep(&state).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(clip_jobs(&pool, earn).await.len(), 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stale_pending_is_re_enqueued_once(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121172).await;

        backdate_updated_at(&pool, earn).await;
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(clip_jobs(&pool, earn).await.len(), 2, "one re-drive job");
        // The updated_at bump means the very next tick re-drives nothing.
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(clip_jobs(&pool, earn).await.len(), 2);

        render_achievement_clip(&state, earn).await.unwrap();
        assert_eq!(clip(&pool, earn).await.unwrap().status, "rendered");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stranded_pending_at_cap_marked_failed_by_sweep(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let owner = user(&pool, 2121173).await;
        let owner_snake = snake(&pool, owner, "stranded owner").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1).await;
        seed_turn(&pool, game_id, 0).await;
        let earn = seed_live_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(0),
        )
        .await;
        // A process death during attempt 3 leaves this exact row.
        sqlx::query(
            "INSERT INTO achievement_clips (achievement_earn_id, status, attempts) VALUES ($1, 'pending', 3)",
        )
        .bind(earn)
        .execute(&pool)
        .await
        .unwrap();
        backdate_updated_at(&pool, earn).await;

        run_clip_sweep(&state).await.unwrap();
        assert_eq!(clip(&pool, earn).await.unwrap().status, "failed");
        let events = clip_events(earn);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].get("result").map(String::as_str), Some("failed"));
        assert_eq!(events[0].get("ms").map(String::as_str), Some("0"));
        assert_eq!(events[0].get("bytes").map(String::as_str), Some("0"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_game_between_sweep_and_render_fails_terminally(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn_setup(&pool, &state, 2121174).await;
        let game_id = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT game_id FROM achievement_earns WHERE achievement_earn_id = $1",
        )
        .bind(earn)
        .fetch_one(&pool)
        .await
        .unwrap()
        .expect("earn has a game");
        sqlx::query("DELETE FROM games WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();

        render_achievement_clip(&state, earn).await.unwrap();
        let row = clip(&pool, earn).await.unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(store.put_count(), 0);
        let events = clip_events(earn);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].get("result").map(String::as_str), Some("failed"));
        // No loop: the sweep never re-drives a failed row.
        backdate_updated_at(&pool, earn).await;
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(clip_jobs(&pool, earn).await.len(), 1, "only the first job");
    }

    // ---- pruned game via archive ----

    #[sqlx::test(migrations = "../migrations")]
    async fn renders_pruned_game_from_archive(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        use crate::routes::game::api::fixtures as fx;

        let owner = user(&pool, 2121181).await;
        let owner_snake = snake(&pool, owner, "pruned game owner").await;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let gcs_path = format!("native/2026/11/05/{game_id}.json.zst");
        sqlx::query(
            "UPDATE games SET archived_at = NOW(), gcs_path = $1, archive_version = 2 WHERE game_id = $2",
        )
        .bind(&gcs_path)
        .bind(game_id)
        .execute(&pool)
        .await
        .unwrap();
        participant(&pool, game_id, owner_snake, 1).await;
        // NO turns rows: pruned. The archive object carries 0..=40.
        let frames: Vec<serde_json::Value> = (0..=40)
            .map(|turn| fx::native_frame_json(turn, "pruned-snake"))
            .collect();
        let object = serde_json::json!({
            "archive_version": 2,
            "game_id": game_id.to_string(),
            "game": {"game_type": "Standard", "board_size": "11x11", "status": "finished"},
            "frames": frames,
            "exported_at": "2026-11-05T00:00:00Z",
        });
        store
            .put(&gcs_path, fx::zstd_json(&object, 9))
            .await
            .unwrap();

        let earn = seed_live_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(40),
        )
        .await;
        run_clip_sweep(&state).await.unwrap();
        render_achievement_clip(&state, earn).await.unwrap();

        assert_eq!(clip(&pool, earn).await.unwrap().status, "rendered");
        let bytes = store
            .get(&clip_object_path(earn))
            .await
            .unwrap()
            .expect("stored clip");
        let (_, _, decoded) = decode_gif(&bytes);
        assert_eq!(decoded.len(), 21, "window 20..=40");
    }

    // ---- sweep predicate vs game_is_hidden ----

    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_predicate_matches_game_is_hidden(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;

        // Four games: public; private leaderboard link; private
        // participant entry; non-public tournament match.
        let mut earns = Vec::new();
        for number in 0..4 {
            let owner = user(&pool, 2121191 + number).await;
            let owner_snake = snake(&pool, owner, "predicate snake").await;
            let game_id = game(&pool, "finished").await;
            participant(&pool, game_id, owner_snake, 1).await;
            match number {
                1 => {
                    let private_lb = crate::models::private_leaderboard::create(
                        &pool,
                        &crate::models::private_leaderboard::CreatePrivateLeaderboard {
                            name: format!("Hidden {number}"),
                            game_type: "Standard".to_string(),
                            board_size: "11x11".to_string(),
                            match_size: 2,
                        },
                    )
                    .await
                    .unwrap();
                    crate::models::leaderboard::create_leaderboard_game(&pool, private_lb, game_id)
                        .await
                        .unwrap();
                }
                2 => {
                    let rival = user(&pool, 2121195 + number).await;
                    let rival_snake = snake(&pool, rival, "private entry rival").await;
                    let private_lb = crate::models::private_leaderboard::create(
                        &pool,
                        &crate::models::private_leaderboard::CreatePrivateLeaderboard {
                            name: format!("Entry {number}"),
                            game_type: "Standard".to_string(),
                            board_size: "11x11".to_string(),
                            match_size: 2,
                        },
                    )
                    .await
                    .unwrap();
                    let entry = crate::models::leaderboard::get_or_create_entry(
                        &pool,
                        private_lb,
                        rival_snake,
                    )
                    .await
                    .unwrap()
                    .leaderboard_entry_id;
                    sqlx::query(
                        "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement, leaderboard_entry_id) VALUES ($1, $2, 2, $3)",
                    )
                    .bind(game_id)
                    .bind(rival_snake)
                    .bind(entry)
                    .execute(&pool)
                    .await
                    .unwrap();
                }
                3 => {
                    let tournament_id: Uuid = sqlx::query_scalar(
                        "INSERT INTO tournaments (user_id, name, visibility) VALUES ($1, 'Predicate tournament', 'participants_only') RETURNING tournament_id",
                    )
                    .bind(owner)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    let match_id: Uuid = sqlx::query_scalar(
                        "INSERT INTO tournament_matches (tournament_id, round, position, visual_column, visual_row) VALUES ($1, 1, 1, 0, 0) RETURNING match_id",
                    )
                    .bind(tournament_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    sqlx::query("INSERT INTO match_games (match_id, game_id, game_number) VALUES ($1, $2, 1)")
                        .bind(match_id)
                        .bind(game_id)
                        .execute(&pool)
                        .await
                        .unwrap();
                }
                _ => {}
            }
            seed_turn(&pool, game_id, 0).await;
            earns.push((
                seed_live_earn(
                    &pool,
                    owner,
                    "first_win",
                    Some(owner_snake),
                    Some(game_id),
                    Some(0),
                )
                .await,
                game_id,
            ));
        }

        run_clip_sweep(&state).await.unwrap();
        for (earn, game_id) in earns {
            let hidden = crate::routes::achievements::game_is_hidden(&pool, game_id)
                .await
                .unwrap();
            let row = clip(&pool, earn).await;
            assert_eq!(
                row.is_some(),
                !hidden,
                "sweep predicate must agree with game_is_hidden for {game_id}"
            );
        }
    }

    // ---- privacy and page ----

    #[sqlx::test(migrations = "../migrations")]
    async fn private_leaderboard_earn_gets_no_row_render_or_object(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let owner = user(&pool, 2121201).await;
        let owner_snake = snake(&pool, owner, "private lb owner").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1).await;
        seed_turn(&pool, game_id, 0).await;
        let private_lb = crate::models::private_leaderboard::create(
            &pool,
            &crate::models::private_leaderboard::CreatePrivateLeaderboard {
                name: "Clip hidden".to_string(),
                game_type: "Standard".to_string(),
                board_size: "11x11".to_string(),
                match_size: 2,
            },
        )
        .await
        .unwrap();
        crate::models::leaderboard::create_leaderboard_game(&pool, private_lb, game_id)
            .await
            .unwrap();
        let earn = seed_live_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            Some(0),
        )
        .await;

        run_clip_sweep(&state).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(store.put_count(), 0);
        let jobs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE name = 'RenderAchievementClipJob'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 0);

        // A row that slipped in before the game flipped private is deleted
        // by the render job — no render, no object, no event.
        sqlx::query(
            "INSERT INTO achievement_clips (achievement_earn_id, status) VALUES ($1, 'pending')",
        )
        .bind(earn)
        .execute(&pool)
        .await
        .unwrap();
        render_achievement_clip(&state, earn).await.unwrap();
        assert!(clip(&pool, earn).await.is_none(), "row deleted");
        assert_eq!(store.put_count(), 0);
        assert!(clip_events(earn).is_empty());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn clip_route_rechecks_privacy_at_serve_time(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let earn = rendered_public_earn(&pool, &state, 2121211, 0..=12, 9).await;
        let owner = earn_owner(&pool, earn).await;
        let game_id = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT game_id FROM achievement_earns WHERE achievement_earn_id = $1",
        )
        .bind(earn)
        .fetch_one(&pool)
        .await
        .unwrap()
        .expect("earn has a game");
        let uri = format!("/achievements/first_win/{owner}/clip.gif");
        let stored = store
            .get(&clip_object_path(earn))
            .await
            .unwrap()
            .expect("stored clip");

        let reply = get(&state, &uri).await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        assert_eq!(reply.content_type.as_deref(), Some("image/gif"));
        assert_eq!(reply.cache_control.as_deref(), Some("private, max-age=300"));
        assert_eq!(reply.body, stored);

        // The game flips private: same URL, plain 404.
        let private_lb = crate::models::private_leaderboard::create(
            &pool,
            &crate::models::private_leaderboard::CreatePrivateLeaderboard {
                name: "Serve-time hidden".to_string(),
                game_type: "Standard".to_string(),
                board_size: "11x11".to_string(),
                match_size: 2,
            },
        )
        .await
        .unwrap();
        crate::models::leaderboard::create_leaderboard_game(&pool, private_lb, game_id)
            .await
            .unwrap();
        let reply = get(&state, &uri).await;
        assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);

        // The game is deleted: the route 404s and the page drops the clip
        // (the earn's game_id is now NULL).
        sqlx::query("UPDATE leaderboards SET visibility = 'public', invite_code = NULL WHERE leaderboard_id = $1")
            .bind(private_lb)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM games WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(&state, &uri).await;
        assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
        let view = crate::routes::achievements::load_share_view(
            &state.db,
            owner,
            crate::customizations::achievements::Achievement::FirstWin,
        )
        .await
        .unwrap()
        .expect("page survives a deleted game");
        assert!(!view.clip_ready);
        let reply = get(&state, &format!("/achievements/first_win/{owner}")).await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        let body = String::from_utf8_lossy(&reply.body);
        assert!(!body.contains("ach-share-clip"));
        assert!(!body.contains("clip.gif"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn clip_markup_on_share_page(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let earn = rendered_public_earn(&pool, &state, 2121221, 0..=12, 9).await;
        let owner = earn_owner(&pool, earn).await;
        let page_uri = format!("/achievements/first_win/{owner}");

        // Visitor: the clip shows, no download, og:image stays the PNG card.
        let reply = get(&state, &page_uri).await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        let body = String::from_utf8_lossy(&reply.body).into_owned();
        assert!(body.contains("figure class=\"ach-share-clip\""));
        assert!(body.contains(&format!("src=\"/achievements/first_win/{owner}/clip.gif\"")));
        assert!(!body.contains("Download Clip"));
        assert!(body.contains(&format!("/og/achievements/first_win/{owner}.png")));

        // Owner: Download Clip wired to the same share-action POST.
        let cookie = session_cookie(&state, &pool, owner).await;
        let reply = request(
            &state,
            "GET",
            &page_uri,
            vec![(axum::http::header::COOKIE.as_str(), cookie)],
            None,
        )
        .await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        let body = String::from_utf8_lossy(&reply.body).into_owned();
        assert!(body.contains("Download Clip"));
        assert!(body.contains("data-share-download"));
        assert!(
            body.contains("id=\"share-download\""),
            "card link keeps its id"
        );

        // Pending and failed clips look finished: no figure, no clip URL.
        let pending_owner = user(&pool, 2121222).await;
        let pending_snake = snake(&pool, pending_owner, "pending clip owner").await;
        let pending_game = game(&pool, "finished").await;
        participant(&pool, pending_game, pending_snake, 1).await;
        let pending_earn = seed_live_earn(
            &pool,
            pending_owner,
            "crowd_pleaser",
            Some(pending_snake),
            Some(pending_game),
            Some(0),
        )
        .await;
        sqlx::query(
            "INSERT INTO achievement_clips (achievement_earn_id, status) VALUES ($1, 'pending')",
        )
        .bind(pending_earn)
        .execute(&pool)
        .await
        .unwrap();
        let failed_owner = user(&pool, 2121223).await;
        let failed_snake = snake(&pool, failed_owner, "failed clip owner").await;
        let failed_game = game(&pool, "finished").await;
        participant(&pool, failed_game, failed_snake, 1).await;
        let failed_earn = seed_live_earn(
            &pool,
            failed_owner,
            "crowd_pleaser",
            Some(failed_snake),
            Some(failed_game),
            Some(0),
        )
        .await;
        sqlx::query("INSERT INTO achievement_clips (achievement_earn_id, status, attempts) VALUES ($1, 'failed', 3)")
            .bind(failed_earn)
            .execute(&pool)
            .await
            .unwrap();
        for owner_id in [pending_owner, failed_owner] {
            let reply = get(&state, &format!("/achievements/crowd_pleaser/{owner_id}")).await;
            assert_eq!(reply.status, axum::http::StatusCode::OK);
            let body = String::from_utf8_lossy(&reply.body);
            assert!(!body.contains("ach-share-clip"), "no clip figure");
            assert!(!body.contains("clip.gif"), "no clip URL anywhere");
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleting_the_user_removes_the_clip(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let earn = rendered_public_earn(&pool, &state, 2121231, 0..=12, 9).await;
        let owner = earn_owner(&pool, earn).await;
        let uri = format!("/achievements/first_win/{owner}/clip.gif");
        assert_eq!(get(&state, &uri).await.status, axum::http::StatusCode::OK);

        sqlx::query("DELETE FROM users WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        assert!(clip(&pool, earn).await.is_none(), "cascade removed the row");
        let reply = get(&state, &uri).await;
        assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unknown_pending_failed_and_malformed_clip_requests_404_alike(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let unknown = user(&pool, 2121241).await;
        let no_row = user(&pool, 2121242).await;
        let pending_owner = user(&pool, 2121243).await;
        let pending_snake = snake(&pool, pending_owner, "404 pending").await;
        let pending_game = game(&pool, "finished").await;
        participant(&pool, pending_game, pending_snake, 1).await;
        let pending_earn = seed_live_earn(
            &pool,
            pending_owner,
            "first_win",
            Some(pending_snake),
            Some(pending_game),
            Some(0),
        )
        .await;
        sqlx::query(
            "INSERT INTO achievement_clips (achievement_earn_id, status) VALUES ($1, 'pending')",
        )
        .bind(pending_earn)
        .execute(&pool)
        .await
        .unwrap();
        let failed_owner = user(&pool, 2121244).await;
        let failed_snake = snake(&pool, failed_owner, "404 failed").await;
        let failed_game = game(&pool, "finished").await;
        participant(&pool, failed_game, failed_snake, 1).await;
        let failed_earn = seed_live_earn(
            &pool,
            failed_owner,
            "first_win",
            Some(failed_snake),
            Some(failed_game),
            Some(0),
        )
        .await;
        sqlx::query("INSERT INTO achievement_clips (achievement_earn_id, status, attempts) VALUES ($1, 'failed', 3)")
            .bind(failed_earn)
            .execute(&pool)
            .await
            .unwrap();

        for uri in [
            format!("/achievements/nope/{unknown}/clip.gif"),
            format!("/achievements/first_win/{unknown}/clip.gif"),
            "/achievements/first_win/not-a-uuid/clip.gif".to_string(),
            format!("/achievements/first_win/{no_row}/clip.gif"),
            format!("/achievements/first_win/{pending_owner}/clip.gif"),
            format!("/achievements/first_win/{failed_owner}/clip.gif"),
        ] {
            let reply = get(&state, &uri).await;
            assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND, "{uri}");
            assert!(reply.body.is_empty(), "{uri} must be a bare 404");
        }
    }

    // ---- nothing at view time ----

    #[sqlx::test(migrations = "../migrations")]
    async fn view_time_never_renders(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, store) = clip_state(&pool).await;
        let own = rendered_public_earn(&pool, &state, 2121251, 0..=12, 9).await;
        let owner = earn_owner(&pool, own).await;
        assert_eq!(calls(own), 1);
        // Poison AFTER setup: any render attempt from here panics loudly.
        seam(|s| {
            s.poisoned.insert(own);
        });

        let page_uri = format!("/achievements/first_win/{owner}");
        let clip_uri = format!("{page_uri}/clip.gif");
        // Visitor page.
        let reply = request(
            &state,
            "GET",
            &page_uri,
            vec![(
                axum::http::header::USER_AGENT.as_str(),
                BROWSER_UA.to_string(),
            )],
            None,
        )
        .await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        // Owner page (signed cookie).
        let cookie = session_cookie(&state, &pool, owner).await;
        let reply = request(
            &state,
            "GET",
            &page_uri,
            vec![(axum::http::header::COOKIE.as_str(), cookie)],
            None,
        )
        .await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        // Stored clip route.
        let reply = get(&state, &clip_uri).await;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        assert!(!reply.body.is_empty());
        // Share actions.
        for (endpoint, body) in [
            ("share-action", "channel=download"),
            ("cta-click", "cta=achievements"),
        ] {
            let reply = request(
                &state,
                "POST",
                &format!("{page_uri}/{endpoint}"),
                vec![],
                Some(body),
            )
            .await;
            assert_eq!(reply.status, axum::http::StatusCode::NO_CONTENT);
        }
        assert_eq!(calls(own), 1, "viewing and sharing never render");
        assert_eq!(store.put_count(), 1, "and never write an object");

        // Control: the seam does fire on a real render (a second earn).
        seam(|s| {
            s.poisoned.remove(&own);
        });
        let second = rendered_public_earn(&pool, &state, 2121252, 0..=12, 9).await;
        assert_eq!(calls(second), 1);
    }

    // ---- award path ----

    #[sqlx::test(migrations = "../migrations")]
    async fn award_path_unchanged_by_clips(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let (state, _store) = clip_state(&pool).await;
        let winner = user(&pool, 2121261).await;
        let winner_snake = snake(&pool, winner, "award path winner").await;
        let loser = user(&pool, 2121262).await;
        let loser_snake = snake(&pool, loser, "award path loser").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, winner_snake, 1).await;
        participant(&pool, game_id, loser_snake, 2).await;
        for turn in 0..=12 {
            seed_turn(&pool, game_id, turn).await;
        }

        crate::customizations::achievements::award_for_game(
            &state.db,
            game_id,
            &state.config.base_url,
        )
        .await
        .unwrap();

        // The earn landed and no clip machinery moved: the award path has
        // no new way to fail and no coupling to clips.
        let earns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM achievement_earns WHERE user_id = $1 AND achievement = 'first_win'",
        )
        .bind(winner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(earns, 1);
        let clips: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_clips")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(clips, 0, "no clip row from the award path");
        let jobs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE name = 'RenderAchievementClipJob'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 0);

        // The sweep, not the award, creates the row.
        run_clip_sweep(&state).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM achievement_clips")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    // ---- worst-case benchmark (manual: cargo test --release -- --ignored clip_bench) ----

    /// CPU time (user+sys) of this process in seconds, from /proc/self/stat.
    fn cpu_seconds() -> f64 {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let fields: Vec<&str> = stat.split_whitespace().collect();
        let ticks = |i: usize| {
            fields
                .get(i)
                .and_then(|f| f.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let hz = 100.0; // _SC_CLK_TCK on Linux/aarch64
        (ticks(13) + ticks(14)) as f64 / hz
    }

    /// VmHWM (peak RSS) in MB from /proc/self/status.
    fn peak_rss_mb() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:").map(|kb| {
                    kb.trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<f64>()
                        .unwrap_or(0.0)
                        / 1024.0
                })
            })
            .unwrap_or(0.0)
    }

    /// The pixel at the center of board cell (cx, cy) for a `side`-wide
    /// board panel, mirroring Canvas::board's layout math.
    fn cell_center_pixel(board_side: i32, cx: i32, cy: i32) -> (u32, u32) {
        // Board::new adds a CELL margin on each side; board.px_width is
        // cells * CELL + 2 * GAP-ish margins — read the constant the same
        // way the painter does by scaling the full canvas.
        let cells = board_side.max(1);
        let px = f64::from(CLIP_SIDE) / f64::from(cells) / 2.0;
        (
            (f64::from(cx) * f64::from(CLIP_SIDE) / f64::from(cells) + px) as u32,
            (f64::from(cy) * f64::from(CLIP_SIDE) / f64::from(cells) + px) as u32,
        )
    }

    /// Manual, ignored: renders and encodes the two benchmark cases with
    /// the real painter and reports CPU seconds, peak RSS, output bytes,
    /// and per-frame NeuQuant shimmer at an always-empty cell. Writes the
    /// GIFs to $CLIP_BENCH_OUT when set.
    #[test]
    #[ignore = "manual benchmark: cargo test --release -- --ignored clip_bench"]
    fn clip_bench() {
        /// A 4-snake frame with serpentine bodies of `length` segments on
        /// a `side`-wide board, real catalog heads and tails.
        fn bench_frame(side: i32, length: usize) -> BoardArt {
            let heads = ["crystal-power", "dragon", "gamer", "sand-worm"];
            let tails = ["bolt", "alligator", "bee", "block-bum"];
            let colors = ["#ff3d8a", "#38bdf8", "#a3e635", "#fbbf24"];
            let snakes = heads
                .iter()
                .zip(tails.iter().zip(colors.iter()))
                .enumerate()
                .map(|(i, (head, (tail, color)))| {
                    let body: Vec<(i32, i32)> = (0..length)
                        .map(|k| {
                            // Vertical serpentine starting at column 2+i*n:
                            // columns 0-1 stay empty in every frame, so the
                            // probe cell (1, side/2) is constant.
                            let column = 2 + (i as i32) * ((side - 2) / 5) + (k as i32 / side);
                            let row = (k as i32) % side;
                            (
                                column,
                                if (k / side as usize) % 2 == 0 {
                                    row
                                } else {
                                    side - 1 - row
                                },
                            )
                        })
                        .collect();
                    crate::og::board::BoardSnake {
                        body,
                        color: crate::og::canvas::snake_color(color),
                        head: crate::og::board::shape(arena::design_kit::AssetKind::Head, head),
                        tail: crate::og::board::shape(arena::design_kit::AssetKind::Tail, tail),
                        eliminated: false,
                    }
                })
                .collect();
            BoardArt {
                width: side,
                height: side,
                snakes,
                food: vec![(0, 0), (side - 1, side - 1)],
                hazards: Vec::new(),
            }
        }

        let out_dir = std::env::var("CLIP_BENCH_OUT").ok();
        if let Some(dir) = &out_dir {
            std::fs::create_dir_all(dir).unwrap();
        }
        for (name, side, length) in [
            ("worst-25x25-4x200", 25, 200),
            ("prod-today-11x11-4x5", 11, 5),
        ] {
            let arts: Vec<BoardArt> = (0..26).map(|_turn| bench_frame(side, length)).collect();
            // The always-empty probe cell: column 1 stays clear in every
            // frame (bodies start at column 2), food sits in the corners.
            let probe = cell_center_pixel(side, 1, side / 2);
            let raw_probe: [u8; 4] = {
                let rgba = frame_rgba(&arts[0], CLIP_SIDE).unwrap();
                let idx = (probe.1 as usize * usize::from(CLIP_SIDE) + probe.0 as usize) * 4;
                [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
            };
            assert_eq!(
                raw_probe,
                {
                    let rgba = frame_rgba(&arts[25], CLIP_SIDE).unwrap();
                    let idx = (probe.1 as usize * usize::from(CLIP_SIDE) + probe.0 as usize) * 4;
                    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
                },
                "probe cell must be constant across source frames"
            );

            let cpu_before = cpu_seconds();
            let bytes = encode_clip_gif(&arts, CLIP_SIDE, CLIP_FRAME_DELAY_CS).unwrap();
            let cpu_after = cpu_seconds();
            println!(
                "{name}: {} bytes, {:.2} CPU s (delta), peak RSS {:.0} MB",
                bytes.len(),
                cpu_after - cpu_before,
                peak_rss_mb()
            );

            // Shimmer: max per-channel spread at the probe pixel across
            // the decoded frames (per-frame palettes can disagree).
            let (_, _, frames) = decode_gif(&bytes);
            let mut spread = [0i32; 4];
            for (_, frame) in &frames {
                let idx = (probe.1 as usize * usize::from(CLIP_SIDE) + probe.0 as usize) * 4;
                for channel in 0..4 {
                    let delta = i32::from(frame[idx + channel]) - i32::from(raw_probe[channel]);
                    spread[channel] = spread[channel].max(delta.abs());
                }
            }
            println!(
                "{name}: max per-channel shimmer at empty cell: {:?} (of 255)",
                spread
            );

            if let Some(dir) = &out_dir {
                let path = std::path::Path::new(dir).join(format!("{name}.gif"));
                std::fs::write(&path, &bytes).unwrap();
                println!("{name}: wrote {}", path.display());
            }
        }
    }
}
