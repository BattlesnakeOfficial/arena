//! Read side of the game archive (`backup.rs` is the Play/engine-DB write
//! side, DEV-1980's `native_backup.rs` the native write side): serves
//! archived games (imported Play stubs, pruned native games) to the public
//! game endpoints through the same shapes as DB-backed games.
//!
//! Three object shapes, one decode path: v1/v2 Play (`GameExport`,
//! `EngineGame`-shaped `game`) and v2-family native (`NativeGameExport`,
//! `NativeGameHeader` `game` — ignored on read). The ROW decides which
//! (`ArchiveRowKind`: `engine_game_id` set ⇒ Play). Cached representation
//! is raw JSON text (`Box<RawValue>`), so the cache weight is real bytes;
//! parsing happens per request for the frames served.

use std::collections::HashMap;
use std::sync::Arc;

use color_eyre::eyre::Context as _;
use uuid::Uuid;

/// Default for env `ARCHIVE_CACHE_MAX_BYTES`.
pub const DEFAULT_ARCHIVE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Hard ceilings on one object: compressed before decompression (bounds a
/// zip-bomb's input), decompressed after. Legit archives are KBs-to-low-MBs
/// (~200x below either cap).
const MAX_ARCHIVE_COMPRESSED_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARCHIVE_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Per-entry overhead in the cache weight: a Box pointer per Vec slot plus
/// allocator rounding per allocation.
const RAW_ENTRY_OVERHEAD: usize = 16;

/// Caps concurrent archive CPU work (decode + page parse) on the 1-vCPU
/// prod box. Never shared with `backup.rs`'s `BACKUP_SLOT`, never held
/// across the storage fetch.
static ARCHIVE_DECODE_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// How a `games` row's archive object is shaped. Computed from the row —
/// `engine_game_id IS NOT NULL` ⇒ Play — never from the object, and no
/// UPDATE writes `engine_game_id`, so the kind is immutable per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveRowKind {
    /// `engine_game_id IS NOT NULL` — a `GameExport` (v1 or v2) object.
    Play,
    /// `engine_game_id IS NULL` — a `NativeGameExport` (DEV-1980) object.
    Native,
}

/// One decoded archive object: raw JSON text, nothing parsed into trees.
/// Parsed `serde_json::Value` trees cost 15-30x their text, so the cache
/// never stores them; serve-time parsing works from this raw text.
#[derive(Debug)]
pub struct ArchivedGame {
    game: Box<serde_json::value::RawValue>,
    frames: Vec<Box<serde_json::value::RawValue>>,
    /// Sum of stored raw text lengths + per-entry overhead; the cache weight.
    cached_bytes: usize,
}

impl ArchivedGame {
    /// Parse the engine game header — Play rows only (native rows never
    /// call this; their `Game` comes from the arena row).
    pub fn parse_game(&self) -> cja::Result<crate::engine_models::EngineGame> {
        serde_json::from_str(self.game.get()).wrap_err("Failed to parse archived game header")
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn last_frame(&self) -> Option<&serde_json::value::RawValue> {
        self.frames.last().map(|v| &**v)
    }

    /// All frames, in turn order — the websocket replay path.
    pub fn frames(&self) -> &[Box<serde_json::value::RawValue>] {
        &self.frames
    }

    /// Frames `[offset, offset+limit)` clamped to the frame count.
    /// Saturating arithmetic: a `usize::MAX` limit (or any huge
    /// offset+limit) can never overflow.
    pub fn frame_slice(&self, offset: usize, limit: usize) -> &[Box<serde_json::value::RawValue>] {
        let len = self.frames.len();
        let start = offset.min(len);
        let end = offset.saturating_add(limit).min(len);
        &self.frames[start..end]
    }

    pub fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }
}

/// Load failure classification for callers: 404 vs 500.
#[derive(Debug)]
pub enum LoadArchiveError {
    /// The games row points at an object absent from the bucket: archive
    /// data loss. Logged as ERROR inside `load`; callers serve 404.
    ObjectMissing { path: String },
    /// Storage failure, timeout, or a corrupt/oversized object: 500.
    Failed(cja::color_eyre::Report),
}

/// Decode target. Unknown top-level fields (v1's `exported_at`; v2's
/// `archive_version`, `game_id`, `exported_at`) are ignored by serde's
/// default tolerance, and `game` is captured as raw text whatever shape it
/// carries — that IS the v1/v2/native compatibility.
#[derive(serde::Deserialize)]
struct ArchivedExport {
    game: Box<serde_json::value::RawValue>,
    frames: Vec<Box<serde_json::value::RawValue>>,
}

/// Decompress one archive object and capture its members as raw text;
/// handles Play v1/v2 and native objects alike — the `game` member is
/// validated only for Play rows (native headers are not EngineGame-shaped
/// and are ignored on read). Pure CPU — callers run it on the blocking
/// pool under [`ARCHIVE_DECODE_SLOT`], never an async worker.
fn decode_archive_object(compressed: &[u8], kind: ArchiveRowKind) -> cja::Result<ArchivedGame> {
    if compressed.len() > MAX_ARCHIVE_COMPRESSED_BYTES {
        return Err(cja::color_eyre::eyre::eyre!(
            "Archive object exceeds the compressed-size cap ({} > {} bytes)",
            compressed.len(),
            MAX_ARCHIVE_COMPRESSED_BYTES
        ));
    }
    let json = zstd::decode_all(compressed).wrap_err("Failed to decompress archive object")?;
    if json.len() > MAX_ARCHIVE_DECOMPRESSED_BYTES {
        return Err(cja::color_eyre::eyre::eyre!(
            "Archive object exceeds the decompressed-size cap ({} > {} bytes)",
            json.len(),
            MAX_ARCHIVE_DECOMPRESSED_BYTES
        ));
    }
    let export: ArchivedExport =
        serde_json::from_slice(&json).wrap_err("Failed to parse archive object")?;
    // Validate the game header's shape once — Play objects only, AT DECODE
    // (not serve): keeps serve-time parse_game unable to fail on cached
    // data and guarantees a bad-header Play object is never cached (the
    // error path returns before any cache insert).
    if matches!(kind, ArchiveRowKind::Play) {
        serde_json::from_str::<crate::engine_models::EngineGame>(export.game.get())
            .wrap_err("Archived game header has an invalid shape")?;
    }
    let cached_bytes = export.game.get().len()
        + RAW_ENTRY_OVERHEAD
        + export
            .frames
            .iter()
            .map(|f| f.get().len() + RAW_ENTRY_OVERHEAD)
            .sum::<usize>();
    Ok(ArchivedGame {
        game: export.game,
        frames: export.frames,
        cached_bytes,
    })
}

/// Cache state guarded by a `std::sync::Mutex` with poison recovery.
struct CacheState {
    entries: HashMap<Uuid, (Arc<ArchivedGame>, usize)>,
    /// Insertion order; front = oldest = evicted first (FIFO).
    order: std::collections::VecDeque<Uuid>,
    /// Per-key async mutexes for singleflight. Removed by
    /// [`InFlightGuard`]'s Drop when the last holder goes away — cancelled
    /// loads can't leak.
    in_flight: HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>,
    bytes: usize,
}

/// Byte-bounded FIFO cache of decoded archive objects keyed by arena
/// `game_id`. No TTL: entries are immutable terminal games, and all mutable
/// serve-time state (authors, suppressed shouts) is applied outside the
/// cache.
pub struct ArchiveCache {
    max_bytes: usize,
    state: std::sync::Mutex<CacheState>,
}

impl ArchiveCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: std::sync::Mutex::new(CacheState {
                entries: HashMap::new(),
                order: std::collections::VecDeque::new(),
                in_flight: HashMap::new(),
                bytes: 0,
            }),
        }
    }

    /// False when the cap is 0 (caching disabled — `load` still serves).
    pub(crate) fn is_enabled(&self) -> bool {
        self.max_bytes > 0
    }

    pub fn peek(&self, game_id: Uuid) -> Option<Arc<ArchivedGame>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.get(&game_id).map(|(value, _)| value.clone())
    }

    /// Insert + evict FIFO while `bytes > max_bytes` (an entry heavier than
    /// the whole cap evicts itself: served now, not retained).
    fn insert(&self, game_id: Uuid, value: Arc<ArchivedGame>, weight: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.entries.contains_key(&game_id) {
            return;
        }
        state.entries.insert(game_id, (value, weight));
        state.order.push_back(game_id);
        state.bytes += weight;
        while state.bytes > self.max_bytes {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            if let Some((_, weight)) = state.entries.remove(&oldest) {
                state.bytes -= weight;
            }
        }
    }

    /// Acquire the per-key lock (inserting it if absent). The returned
    /// guard releases the mutex on drop and removes the map entry iff the
    /// map still holds this exact `Arc` (`Arc::ptr_eq`) and
    /// `Arc::strong_count(&lock) == 2` (map + this guard), checked under
    /// the state mutex.
    async fn lock_key(self: &Arc<Self>, game_id: Uuid) -> InFlightGuard {
        let lock = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .in_flight
                .entry(game_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let guard = lock.clone().lock_owned().await;
        InFlightGuard {
            cache: self.clone(),
            key: game_id,
            lock,
            guard: Some(guard),
        }
    }

    #[cfg(test)]
    fn in_flight_len(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.bytes
    }
}

/// RAII holder of a per-key load mutex: the load future holds this across
/// its awaits, so same-game fetches coalesce, and the `in_flight` map entry
/// is cleaned up even when the future is cancelled mid-fetch.
pub(crate) struct InFlightGuard {
    cache: Arc<ArchiveCache>,
    key: Uuid,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Release the async mutex BEFORE counting: the OwnedMutexGuard
        // holds its own Arc clone, and the `== 2` check below wants exactly
        // map + this struct's `lock` clone.
        self.guard.take();
        let mut state = self.cache.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = state.in_flight.get(&self.key)
            && Arc::ptr_eq(current, &self.lock)
            && Arc::strong_count(&self.lock) == 2
        {
            state.in_flight.remove(&self.key);
        }
    }
}

/// Reads archived games from object storage through the [`crate::object_store::ObjectStore`]
/// seam, caching decoded objects (bounded in cache-resident raw-text bytes).
#[derive(Clone)]
pub struct ArchiveReader {
    store: Arc<dyn crate::object_store::ObjectStore>,
    cache: Arc<ArchiveCache>,
    fetch_timeout: std::time::Duration,
}

impl ArchiveReader {
    pub fn new(store: Arc<dyn crate::object_store::ObjectStore>, max_cache_bytes: usize) -> Self {
        Self {
            store,
            cache: Arc::new(ArchiveCache::new(max_cache_bytes)),
            fetch_timeout: std::time::Duration::from_secs(10),
        }
    }

    /// Tests only: exercise the timeout arm without waiting 10 s.
    #[cfg(test)]
    pub fn with_fetch_timeout(mut self, d: std::time::Duration) -> Self {
        self.fetch_timeout = d;
        self
    }

    /// Load one game's decoded archive, from cache when cached. Same-game
    /// loads coalesce on a per-key mutex (singleflight); nothing partial is
    /// ever cached — insertion happens only after fetch + decode fully
    /// succeed. A missing object is logged as ERROR (archive data loss) and
    /// returned as [`LoadArchiveError::ObjectMissing`].
    pub async fn load(
        &self,
        game_id: Uuid,
        gcs_path: &str,
        kind: ArchiveRowKind,
    ) -> Result<Arc<ArchivedGame>, LoadArchiveError> {
        if let Some(hit) = self.cache.peek(game_id) {
            return Ok(hit);
        }
        if !self.cache.is_enabled() {
            let archived = self.fetch_and_decode(game_id, gcs_path, kind).await?;
            return Ok(Arc::new(archived));
        }
        let _guard = self.cache.lock_key(game_id).await;
        if let Some(hit) = self.cache.peek(game_id) {
            return Ok(hit);
        }
        let archived = self.fetch_and_decode(game_id, gcs_path, kind).await?;
        let weight = archived.cached_bytes();
        let archived = Arc::new(archived);
        self.cache.insert(game_id, archived.clone(), weight);
        Ok(archived)
    }

    async fn fetch_and_decode(
        &self,
        game_id: Uuid,
        gcs_path: &str,
        kind: ArchiveRowKind,
    ) -> Result<ArchivedGame, LoadArchiveError> {
        let fetch_started = std::time::Instant::now();
        let compressed = tokio::time::timeout(self.fetch_timeout, self.store.get(gcs_path))
            .await
            .map_err(|_| {
                LoadArchiveError::Failed(cja::color_eyre::eyre::eyre!(
                    "Archive fetch timed out after {:?} for game {game_id}",
                    self.fetch_timeout
                ))
            })?
            .map_err(LoadArchiveError::Failed)?;
        let fetch_ms = fetch_started.elapsed().as_millis() as u64;
        let Some(compressed) = compressed else {
            tracing::error!(
                game_id = %game_id,
                path = gcs_path,
                "Archived game object missing from bucket — archive data loss"
            );
            return Err(LoadArchiveError::ObjectMissing {
                path: gcs_path.to_string(),
            });
        };
        let compressed_bytes = compressed.len();
        let decode_started = std::time::Instant::now();
        // The permit is moved into the closure: no cancellation path frees
        // the slot mid-decode.
        let permit = ARCHIVE_DECODE_SLOT.acquire().await.map_err(|_| {
            LoadArchiveError::Failed(cja::color_eyre::eyre::eyre!("Archive decode slot closed"))
        })?;
        let decoded = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_archive_object(&compressed, kind)
        })
        .await
        .map_err(|e| {
            LoadArchiveError::Failed(cja::color_eyre::eyre::eyre!(
                "Archive decode task panicked: {e}"
            ))
        })?
        .map_err(LoadArchiveError::Failed)?;
        let decode_ms = decode_started.elapsed().as_millis() as u64;
        tracing::info!(
            game_id = %game_id,
            path = gcs_path,
            compressed_bytes,
            cached_bytes = decoded.cached_bytes(),
            fetch_ms,
            decode_ms,
            "Served game from archive"
        );
        Ok(decoded)
    }

    /// Parse one raw frame to a `Value` and run the same public prep as DB
    /// frames (author fill, suppressed-shout strip, `normalize_public_frame`).
    /// Works identically for Play frames (EngineGameFrame-shaped) and
    /// native frames (raw `turns.frame_data`) — both are the engine's
    /// public PascalCase frame shape.
    pub(crate) fn prepare_frame(
        raw: &serde_json::value::RawValue,
        authors: &HashMap<String, String>,
        suppressed: &HashMap<String, std::collections::HashSet<String>>,
    ) -> cja::Result<serde_json::Value> {
        let mut frame: serde_json::Value =
            serde_json::from_str(raw.get()).wrap_err("Failed to parse archived frame")?;
        crate::routes::game::api::prepare_public_frame(&mut frame, authors, suppressed);
        Ok(frame)
    }

    /// Parse + prep a page of frames (<= 100, up to a few hundred KB of
    /// text) off the async workers, under [`ARCHIVE_DECODE_SLOT`].
    pub async fn prepare_frame_page(
        &self,
        archived: Arc<ArchivedGame>,
        offset: usize,
        limit: usize,
        authors: HashMap<String, String>,
        suppressed: HashMap<String, std::collections::HashSet<String>>,
    ) -> cja::Result<Vec<serde_json::Value>> {
        let permit = ARCHIVE_DECODE_SLOT
            .acquire()
            .await
            .wrap_err("Archive decode slot closed")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            archived
                .frame_slice(offset, limit)
                .iter()
                .map(|raw| Self::prepare_frame(raw, &authors, &suppressed))
                .collect::<cja::Result<Vec<_>>>()
        })
        .await
        .wrap_err("Archive page parse task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::InMemoryObjectStore;
    use crate::object_store::ObjectStore as _;
    use crate::routes::game::api::fixtures as fx;

    fn reader(store: Arc<InMemoryObjectStore>, cap: usize) -> ArchiveReader {
        ArchiveReader::new(store, cap)
    }

    #[test]
    fn decode_v1_keeps_raw_text_and_counts_frames() {
        let compressed = fx::v1_archive("11111111-1111-1111-1111-111111111111");
        let archived = decode_archive_object(&compressed, ArchiveRowKind::Play).unwrap();
        assert_eq!(archived.frame_count(), 2);
        assert!(archived.cached_bytes() > 0);
        // Blanking happens at prep, not decode: the raw text still carries
        // the secret URL.
        assert!(archived.frames()[0].get().contains("snake.example.com"));
    }

    #[test]
    fn decode_v2_ignores_unknown_top_level_fields() {
        let compressed = fx::v2_archive("22222222-2222-2222-2222-222222222222");
        let archived = decode_archive_object(&compressed, ArchiveRowKind::Play).unwrap();
        assert_eq!(archived.frame_count(), 2);
        assert!(archived.cached_bytes() > 0);
        let game = archived.parse_game().unwrap();
        assert_eq!(game.width, 15);
    }

    #[test]
    fn decode_native_object_succeeds_without_header_validation() {
        let compressed = fx::native_archive(
            "33333333-3333-3333-3333-333333333333",
            "44444444-4444-4444-4444-444444444444",
        );
        let archived = decode_archive_object(&compressed, ArchiveRowKind::Native).unwrap();
        assert_eq!(archived.frame_count(), 2);
        assert!(archived.cached_bytes() > 0);
    }

    #[test]
    fn decode_native_object_with_play_kind_fails_loudly() {
        let compressed = fx::native_archive(
            "33333333-3333-3333-3333-333333333333",
            "44444444-4444-4444-4444-444444444444",
        );
        // The NativeGameHeader is not EngineGame-shaped: a mis-kindred row
        // 500s loudly instead of serving garbage.
        assert!(decode_archive_object(&compressed, ArchiveRowKind::Play).is_err());
    }

    #[test]
    fn decode_play_object_with_bad_header_shape_fails() {
        let game = serde_json::json!({
            "ID": "g", "Status": "complete", "Width": "not-a-number"
        });
        let object = serde_json::json!({ "game": game, "frames": [] });
        let compressed = fx::zstd_json(&object, 3);
        assert!(decode_archive_object(&compressed, ArchiveRowKind::Play).is_err());
    }

    #[test]
    fn decode_truncated_zstd_fails() {
        let compressed = fx::v1_archive("55555555-5555-5555-5555-555555555555");
        let truncated = &compressed[..compressed.len() / 2];
        assert!(decode_archive_object(truncated, ArchiveRowKind::Play).is_err());
    }

    #[test]
    fn decode_rejects_oversized_input_before_decompressing() {
        let oversized = vec![0u8; MAX_ARCHIVE_COMPRESSED_BYTES + 1];
        assert!(decode_archive_object(&oversized, ArchiveRowKind::Play).is_err());
    }

    #[test]
    fn cached_weight_is_stored_bytes() {
        let compressed = fx::v1_archive("66666666-6666-6666-6666-666666666666");
        let archived = decode_archive_object(&compressed, ArchiveRowKind::Play).unwrap();

        let json = zstd::decode_all(compressed.as_slice()).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let game_len = serde_json::to_string(&value["game"]).unwrap().len();
        let frames_len: usize = value["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| serde_json::to_string(f).unwrap().len() + RAW_ENTRY_OVERHEAD)
            .sum();

        assert_eq!(
            archived.cached_bytes(),
            game_len + RAW_ENTRY_OVERHEAD + frames_len
        );
    }

    #[test]
    fn frames_accessor_and_frame_slice_clamp() {
        let compressed = fx::v1_archive("77777777-7777-7777-7777-777777777777");
        let archived = decode_archive_object(&compressed, ArchiveRowKind::Play).unwrap();
        assert_eq!(archived.frames().len(), archived.frame_count());
        assert_eq!(
            archived.frame_slice(1, usize::MAX).len(),
            archived.frame_count() - 1
        );
        // No overflow panic in debug, no wrap in release.
        assert_eq!(archived.frame_slice(usize::MAX, 1).len(), 0);
        assert_eq!(archived.frame_slice(0, 0).len(), 0);
    }

    #[tokio::test]
    async fn archive_cache_evicts_to_byte_cap() {
        let store = Arc::new(InMemoryObjectStore::default());
        let a = fx::v1_archive("88888888-8888-8888-8888-888888888888");
        // Same fixture shape with a different play id: identical cached
        // weight, so the cap (one entry's weight) retains exactly one.
        let b = fx::v1_archive("99999999-9999-9999-9999-999999999999");
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let weight = decode_archive_object(&a, ArchiveRowKind::Play)
            .unwrap()
            .cached_bytes();
        store.put("a", a).await.unwrap();
        store.put("b", b).await.unwrap();

        let r = reader(store.clone(), weight);
        r.load(id_a, "a", ArchiveRowKind::Play).await.unwrap();
        assert!(r.cache.peek(id_a).is_some());
        r.load(id_b, "b", ArchiveRowKind::Play).await.unwrap();
        assert!(r.cache.peek(id_a).is_none(), "oldest entry evicted");
        assert!(r.cache.peek(id_b).is_some());
        assert!(r.cache.bytes() <= weight);
    }

    #[tokio::test]
    async fn concurrent_loads_of_same_game_fetch_once() {
        let store = Arc::new(InMemoryObjectStore::default());
        store
            .put(
                "games/2026/10/31/x.json.zst",
                fx::v1_archive("aaaaaaaa-1111-1111-1111-111111111111"),
            )
            .await
            .unwrap();
        let r = reader(store.clone(), DEFAULT_ARCHIVE_CACHE_MAX_BYTES);
        let id = Uuid::new_v4();
        let (first, second) = tokio::join!(
            r.load(id, "games/2026/10/31/x.json.zst", ArchiveRowKind::Play),
            r.load(id, "games/2026/10/31/x.json.zst", ArchiveRowKind::Play),
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(store.get_count(), 1);
    }

    /// A store whose `get` pends until notified: lets a load be observed
    /// mid-fetch and then released (or aborted).
    struct PendStore {
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::object_store::ObjectStore for PendStore {
        async fn put(&self, _path: &str, _bytes: Vec<u8>) -> cja::Result<()> {
            Ok(())
        }

        async fn get(&self, _path: &str) -> cja::Result<Option<Vec<u8>>> {
            self.gate.notified().await;
            Ok(None)
        }
    }

    #[tokio::test]
    async fn cancelled_load_releases_in_flight() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let store = Arc::new(PendStore { gate });
        let r = ArchiveReader::new(store, DEFAULT_ARCHIVE_CACHE_MAX_BYTES);
        let id = Uuid::new_v4();

        let reader = r.clone();
        let task = tokio::spawn(async move { reader.load(id, "p", ArchiveRowKind::Play).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while r.cache.in_flight_len() != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while r.cache.in_flight_len() != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fetch_timeout_is_failed() {
        struct NeverStore;
        #[async_trait::async_trait]
        impl crate::object_store::ObjectStore for NeverStore {
            async fn put(&self, _path: &str, _bytes: Vec<u8>) -> cja::Result<()> {
                Ok(())
            }
            async fn get(&self, _path: &str) -> cja::Result<Option<Vec<u8>>> {
                std::future::pending().await
            }
        }
        let r = ArchiveReader::new(Arc::new(NeverStore), DEFAULT_ARCHIVE_CACHE_MAX_BYTES)
            .with_fetch_timeout(std::time::Duration::from_millis(50));
        let started = std::time::Instant::now();
        let result = r
            .load(Uuid::new_v4(), "any", ArchiveRowKind::Play)
            .await
            .unwrap_err();
        assert!(matches!(result, LoadArchiveError::Failed(_)));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn decode_slot_admits_two_at_a_time() {
        let first = ARCHIVE_DECODE_SLOT.acquire().await.unwrap();
        let second = ARCHIVE_DECODE_SLOT.acquire().await.unwrap();
        assert!(ARCHIVE_DECODE_SLOT.try_acquire().is_err());
        drop(first);
        drop(second);
        assert!(ARCHIVE_DECODE_SLOT.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn zero_cap_disables_caching_but_still_serves() {
        let store = Arc::new(InMemoryObjectStore::default());
        store
            .put("z", fx::v1_archive("bbbbbbbb-2222-2222-2222-222222222222"))
            .await
            .unwrap();
        let r = reader(store.clone(), 0);
        let id = Uuid::new_v4();
        let first = r.load(id, "z", ArchiveRowKind::Play).await.unwrap();
        assert_eq!(first.frame_count(), 2);
        assert!(r.cache.peek(id).is_none());
        r.load(id, "z", ArchiveRowKind::Play).await.unwrap();
        assert_eq!(store.get_count(), 2, "no cache: every load re-fetches");
    }
}
