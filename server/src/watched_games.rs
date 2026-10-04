#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, PoisonError},
    time::Duration,
};

use color_eyre::eyre::Context as _;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgListener, PgPoolOptions},
};
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::models::{game::GameStatus, turn::Turn};

#[derive(Debug, Clone)]
pub enum WatchedGameUpdate {
    Frames(Arc<[Turn]>),
    Terminal(GameStatus),
    Missing,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Pending,
    Active { cursor: i32 },
    Retired,
}

pub struct WatchEntry {
    updates: broadcast::Sender<WatchedGameUpdate>,
    readiness: watch::Sender<Readiness>,
    last_turn_id: std::sync::Mutex<Option<Uuid>>,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<Uuid, Arc<WatchEntry>>,
}

#[derive(Clone, Default)]
pub struct WatchedGames {
    registry: Arc<Mutex<Registry>>,
    #[cfg(test)]
    stats: Arc<PollStats>,
}

#[cfg(test)]
#[derive(Default)]
struct PollStats {
    status_reads: AtomicUsize,
    range_reads: AtomicUsize,
    batches: Mutex<Vec<(tokio::time::Instant, Vec<Uuid>)>>,
    reconnect_catchups: AtomicUsize,
    listener_errors: AtomicUsize,
}

pub struct Subscription {
    pub updates: broadcast::Receiver<WatchedGameUpdate>,
    pub readiness: watch::Receiver<Readiness>,
    pub entry: Arc<WatchEntry>,
    pub created: bool,
    pending_guard: Option<PendingGuard>,
}

struct PendingGuard {
    watched: WatchedGames,
    game_id: Uuid,
    entry: Arc<WatchEntry>,
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Wake waiters synchronously. If a poll currently holds the registry,
        // subscribe/snapshot will replace or prune this retired entry later.
        self.entry.readiness.send_replace(Readiness::Retired);
        if let Ok(mut registry) = self.watched.registry.try_lock()
            && registry
                .entries
                .get(&self.game_id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            registry.entries.remove(&self.game_id);
        }
    }
}

impl WatchedGames {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn same_registry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.registry, &other.registry)
    }

    #[cfg(test)]
    pub async fn subscriber_count(&self) -> usize {
        self.registry
            .lock()
            .await
            .entries
            .values()
            .map(|entry| entry.updates.receiver_count())
            .sum()
    }

    #[cfg(test)]
    pub async fn is_active(&self, game_id: Uuid) -> bool {
        self.registry
            .lock()
            .await
            .entries
            .get(&game_id)
            .is_some_and(|entry| matches!(*entry.readiness.borrow(), Readiness::Active { .. }))
    }

    #[cfg(test)]
    pub async fn read_counts(&self) -> (usize, usize) {
        (
            self.stats.status_reads.load(Ordering::Relaxed),
            self.stats.range_reads.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub fn reconnect_catchups(&self) -> usize {
        self.stats.reconnect_catchups.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn listener_errors(&self) -> usize {
        self.stats.listener_errors.load(Ordering::Relaxed)
    }

    pub async fn subscribe(&self, game_id: Uuid) -> Subscription {
        let mut registry = self.registry.lock().await;
        if let Some(entry) = registry.entries.get(&game_id)
            && *entry.readiness.borrow() != Readiness::Retired
        {
            return Subscription {
                updates: entry.updates.subscribe(),
                readiness: entry.readiness.subscribe(),
                entry: entry.clone(),
                created: false,
                pending_guard: None,
            };
        }
        let (updates, receiver) = broadcast::channel(256);
        let (readiness, ready_receiver) = watch::channel(Readiness::Pending);
        let entry = Arc::new(WatchEntry {
            updates,
            readiness,
            last_turn_id: std::sync::Mutex::new(None),
        });
        registry.entries.insert(game_id, entry.clone());
        Subscription {
            updates: receiver,
            readiness: ready_receiver,
            entry: entry.clone(),
            created: true,
            pending_guard: Some(PendingGuard {
                watched: self.clone(),
                game_id,
                entry,
                armed: true,
            }),
        }
    }

    #[cfg(test)]
    pub async fn seed_if_current(
        &self,
        game_id: Uuid,
        subscription: &mut Subscription,
        cursor: i32,
    ) {
        self.seed_with_turn_id(game_id, subscription, cursor, None)
            .await;
    }

    pub async fn seed_with_turn_id(
        &self,
        game_id: Uuid,
        subscription: &mut Subscription,
        cursor: i32,
        turn_id: Option<Uuid>,
    ) {
        let registry = self.registry.lock().await;
        if registry
            .entries
            .get(&game_id)
            .is_some_and(|entry| Arc::ptr_eq(entry, &subscription.entry))
            && *subscription.entry.readiness.borrow() == Readiness::Pending
        {
            *subscription
                .entry
                .last_turn_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = turn_id;
            subscription
                .entry
                .readiness
                .send_replace(Readiness::Active { cursor });
            if let Some(guard) = subscription.pending_guard.as_mut() {
                guard.armed = false;
            }
            subscription.pending_guard = None;
        }
    }

    async fn snapshot(
        &self,
        ids: Option<&HashSet<Uuid>>,
    ) -> Vec<(Uuid, Arc<WatchEntry>, i32, Option<Uuid>)> {
        let mut registry = self.registry.lock().await;
        registry.entries.retain(|_, entry| {
            entry.updates.receiver_count() > 0 && *entry.readiness.borrow() != Readiness::Retired
        });
        registry
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                if ids.is_some_and(|selected| !selected.contains(id)) {
                    return None;
                }
                let Readiness::Active { cursor } = *entry.readiness.borrow() else {
                    return None;
                };
                Some((
                    *id,
                    entry.clone(),
                    cursor,
                    *entry
                        .last_turn_id
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner),
                ))
            })
            .collect()
    }

    async fn retire_resets(&self, ids: &HashSet<Uuid>) {
        let mut registry = self.registry.lock().await;
        for id in ids {
            if let Some(entry) = registry.entries.remove(id) {
                let _ = entry.updates.send(WatchedGameUpdate::Reset);
                entry.readiness.send_replace(Readiness::Retired);
            }
        }
    }
}

// A notification batch makes one range query, plus a status query when needed.
async fn poll_batch(
    pool: &PgPool,
    watched: &WatchedGames,
    snapshot: Vec<(Uuid, Arc<WatchEntry>, i32, Option<Uuid>)>,
    include_status: bool,
    turn_limits: Option<&HashMap<Uuid, i32>>,
) -> cja::Result<()> {
    if snapshot.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = snapshot.iter().map(|(id, _, _, _)| *id).collect();
    let cursors: Vec<i32> = snapshot.iter().map(|(_, _, cursor, _)| *cursor).collect();
    #[cfg(test)]
    watched
        .stats
        .batches
        .lock()
        .await
        .push((tokio::time::Instant::now(), ids.clone()));
    let statuses = if include_status {
        #[cfg(test)]
        watched.stats.status_reads.fetch_add(1, Ordering::Relaxed);
        Some(sqlx::query!(
            r#"SELECT g.game_id, g.status, (SELECT MAX(t.turn_number) FROM turns t WHERE t.game_id = g.game_id) AS latest_turn,
                      c.turn_id AS cursor_turn_id
               FROM unnest($1::uuid[], $2::int[]) AS w(game_id, last_turn)
               JOIN games g ON g.game_id = w.game_id
               LEFT JOIN turns c ON c.game_id = w.game_id AND c.turn_number = w.last_turn"#,
            &ids,
            &cursors
        ).fetch_all(pool).await.wrap_err("Failed to read watched game statuses")?)
    } else {
        None
    };
    #[cfg(test)]
    watched.stats.range_reads.fetch_add(1, Ordering::Relaxed);
    let turns = sqlx::query_as!(
        Turn,
        r#"SELECT t.turn_id, t.game_id, t.turn_number, t.frame_data, t.created_at
           FROM unnest($1::uuid[], $2::int[]) AS w(game_id, last_turn)
           JOIN turns t ON t.game_id = w.game_id
           WHERE t.turn_number > w.last_turn AND t.frame_data IS NOT NULL
           ORDER BY t.game_id, t.turn_number"#,
        &ids,
        &cursors
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to read watched game frames")?;
    let mut grouped: HashMap<Uuid, Vec<Turn>> = HashMap::new();
    for turn in turns {
        // A delayed old-run turn notification must not fetch frames from a
        // newer run before its reset notification is processed.
        if turn_limits
            .and_then(|limits| limits.get(&turn.game_id))
            .is_some_and(|limit| turn.turn_number > *limit)
        {
            continue;
        }
        grouped.entry(turn.game_id).or_default().push(turn);
    }
    let status_map: HashMap<_, _> = statuses
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.game_id, row))
        .collect();

    let mut registry = watched.registry.lock().await;
    for (id, entry, cursor, turn_id) in snapshot {
        if !registry
            .entries
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &entry))
            || *entry.readiness.borrow() != (Readiness::Active { cursor })
            || *entry
                .last_turn_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                != turn_id
        {
            continue;
        }
        let row = status_map.get(&id);
        if include_status && row.is_none() {
            let _ = entry.updates.send(WatchedGameUpdate::Missing);
            entry.readiness.send_replace(Readiness::Retired);
            registry.entries.remove(&id);
            continue;
        }
        // A retry can delete and regrow beyond the old turn number while
        // LISTEN is disconnected. The UUID at that number still changes.
        if row.is_some_and(|row| {
            row.latest_turn.unwrap_or(-1) < cursor
                || (cursor >= 0 && (row.cursor_turn_id.is_none() || row.cursor_turn_id != turn_id))
        }) {
            let _ = entry.updates.send(WatchedGameUpdate::Reset);
            entry.readiness.send_replace(Readiness::Retired);
            registry.entries.remove(&id);
            continue;
        }
        let mut next_cursor = cursor;
        if let Some(frames) = grouped.remove(&id) {
            if let Some(last) = frames.last() {
                next_cursor = last.turn_number;
                *entry
                    .last_turn_id
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(last.turn_id);
            }
            let _ = entry
                .updates
                .send(WatchedGameUpdate::Frames(Arc::from(frames)));
            entry.readiness.send_replace(Readiness::Active {
                cursor: next_cursor,
            });
        }
        let Some(row) = row else { continue };
        match GameStatus::from_str(&row.status) {
            Ok(status @ (GameStatus::Finished | GameStatus::Failed)) => {
                let _ = entry.updates.send(WatchedGameUpdate::Terminal(status));
                entry.readiness.send_replace(Readiness::Retired);
                registry.entries.remove(&id);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%id, error = %format_args!("{error:#}"), "Invalid watched game status")
            }
        }
    }
    Ok(())
}

const LISTENER_RETRY_INITIAL: Duration = Duration::from_millis(100);
const LISTENER_RETRY_MAX: Duration = Duration::from_secs(5);
const CHANNEL: &str = "arena_watched_games";

pub(crate) async fn run_watched_games(
    pool: PgPool,
    listener_options: PgConnectOptions,
    watched: WatchedGames,
    shutdown: CancellationToken,
) -> cja::Result<()> {
    let listener_pool = PgPoolOptions::new()
        .max_connections(1)
        .max_lifetime(None)
        .idle_timeout(None)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with(listener_options.application_name("arena-watched-games-listener"))
        .await
        .wrap_err("Failed to connect watched-game listener")?;
    let mut listener = PgListener::connect_with(&listener_pool)
        .await
        .wrap_err("Failed to acquire watched-game listener")?;
    listener
        .listen(CHANNEL)
        .await
        .wrap_err("Failed to LISTEN for watched games")?;
    catch_up(&pool, &watched).await?;
    let mut retry = LISTENER_RETRY_INITIAL;
    loop {
        let result = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            result = listener.try_recv() => result,
        };
        match result {
            Ok(Some(notification)) => {
                retry = LISTENER_RETRY_INITIAL;
                let mut ids = HashSet::new();
                let mut resets = HashSet::new();
                let mut turn_limits = HashMap::new();
                let mut include_status = false;
                collect_notification(
                    notification.payload(),
                    &mut ids,
                    &mut resets,
                    &mut turn_limits,
                    &mut include_status,
                );
                while let Some(notification) = listener.next_buffered() {
                    collect_notification(
                        notification.payload(),
                        &mut ids,
                        &mut resets,
                        &mut turn_limits,
                        &mut include_status,
                    );
                }
                watched.retire_resets(&resets).await;
                let snapshot = watched.snapshot(Some(&ids)).await;
                if let Err(error) = poll_batch(
                    &pool,
                    &watched,
                    snapshot,
                    include_status,
                    Some(&turn_limits),
                )
                .await
                {
                    tracing::error!(error = %format_args!("{error:#}"), "Watched game notification read failed");
                    reconcile_with_retry(&pool, &watched, &shutdown).await?;
                }
            }
            Ok(None) => {
                // try_recv reconnects and restores LISTEN before returning None.
                // The old session may have missed committed notifications.
                #[cfg(test)]
                watched
                    .stats
                    .reconnect_catchups
                    .fetch_add(1, Ordering::Relaxed);
                reconcile_with_retry(&pool, &watched, &shutdown).await?;
            }
            Err(error) => {
                tracing::error!(error = %format_args!("{error:#}"), "Watched game listener failed");
                #[cfg(test)]
                watched
                    .stats
                    .listener_errors
                    .fetch_add(1, Ordering::Relaxed);
                drop(listener);
                listener = loop {
                    tokio::select! {
                        () = shutdown.cancelled() => return Ok(()),
                        () = tokio::time::sleep(retry) => {},
                    }
                    let rebuild = tokio::select! {
                        () = shutdown.cancelled() => return Ok(()),
                        result = PgListener::connect_with(&listener_pool) => result,
                    };
                    match rebuild {
                        Ok(mut rebuilt) => match tokio::select! {
                            () = shutdown.cancelled() => return Ok(()),
                            result = rebuilt.listen(CHANNEL) => result,
                        } {
                            Ok(()) => break rebuilt,
                            Err(error) => {
                                tracing::error!(error = %format_args!("{error:#}"), "Watched game LISTEN rebuild failed")
                            }
                        },
                        Err(error) => {
                            tracing::error!(error = %format_args!("{error:#}"), "Watched game listener rebuild failed")
                        }
                    }
                    retry = retry.saturating_mul(2).min(LISTENER_RETRY_MAX);
                };
                retry = LISTENER_RETRY_INITIAL;
                #[cfg(test)]
                watched
                    .stats
                    .reconnect_catchups
                    .fetch_add(1, Ordering::Relaxed);
                reconcile_with_retry(&pool, &watched, &shutdown).await?;
            }
        }
    }
}

async fn catch_up(pool: &PgPool, watched: &WatchedGames) -> cja::Result<()> {
    poll_batch(pool, watched, watched.snapshot(None).await, true, None).await
}

async fn reconcile_with_retry(
    pool: &PgPool,
    watched: &WatchedGames,
    shutdown: &CancellationToken,
) -> cja::Result<()> {
    let mut retry = LISTENER_RETRY_INITIAL;
    loop {
        match catch_up(pool, watched).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::error!(error = %format_args!("{error:#}"), "Watched game catch-up failed")
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep(retry) => {},
        }
        retry = retry.saturating_mul(2).min(LISTENER_RETRY_MAX);
    }
}

fn collect_notification(
    payload: &str,
    ids: &mut HashSet<Uuid>,
    resets: &mut HashSet<Uuid>,
    turn_limits: &mut HashMap<Uuid, i32>,
    include_status: &mut bool,
) {
    let mut parts = payload.split(':');
    let Some(kind) = parts.next() else { return };
    let Some(id) = parts.next().and_then(|value| Uuid::parse_str(value).ok()) else {
        return;
    };
    match kind {
        "turn" => {
            if let Some(number) = parts.next().and_then(|value| value.parse::<i32>().ok()) {
                ids.insert(id);
                turn_limits
                    .entry(id)
                    .and_modify(|max| *max = (*max).max(number))
                    .or_insert(number);
            }
        }
        "status" => {
            ids.insert(id);
            *include_status = true;
        }
        "reset" => {
            resets.insert(id);
            *include_status = true;
        }
        _ => tracing::warn!(%payload, "Invalid watched game notification"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_seed_and_creator_drop() {
        let watched = WatchedGames::new();
        let id = Uuid::new_v4();
        let creator = watched.subscribe(id).await;
        let mut joining = watched.subscribe(id).await;
        assert!(creator.created);
        assert!(!joining.created);
        assert_eq!(*joining.readiness.borrow(), Readiness::Pending);
        drop(creator);
        tokio::time::timeout(Duration::from_secs(2), joining.readiness.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*joining.readiness.borrow(), Readiness::Retired);
        assert!(watched.subscribe(id).await.created);
    }

    #[tokio::test]
    async fn seed_does_not_rebroadcast_history_and_subscribers_share_entry() {
        let watched = WatchedGames::new();
        let id = Uuid::new_v4();
        let mut creator = watched.subscribe(id).await;
        let mut others: Vec<_> =
            futures::future::join_all((0..10).map(|_| watched.subscribe(id))).await;
        assert!(others.iter().all(|s| Arc::ptr_eq(&s.entry, &creator.entry)));
        watched.seed_if_current(id, &mut creator, 42).await;
        for subscriber in &mut others {
            subscriber.readiness.changed().await.unwrap();
            assert_eq!(
                *subscriber.readiness.borrow(),
                Readiness::Active { cursor: 42 }
            );
            assert!(matches!(
                subscriber.updates.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn remote_poller_catches_up_and_reports_terminal(pool: PgPool) {
        let id = sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(&pool).await.unwrap();
        let reader = WatchedGames::new();
        let mut subscription = reader.subscribe(id).await;
        reader.seed_if_current(id, &mut subscription, -1).await;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.clone(),
            shutdown.clone(),
        ));
        for turn in 0..3 {
            crate::models::turn::create_turn(
                &pool,
                id,
                turn,
                Some(serde_json::json!({"Turn":turn})),
            )
            .await
            .unwrap();
        }
        let mut received = Vec::new();
        while received.len() < 3 {
            let update = tokio::time::timeout(Duration::from_secs(3), subscription.updates.recv())
                .await
                .unwrap()
                .unwrap();
            if let WatchedGameUpdate::Frames(frames) = update {
                received.extend(frames.iter().map(|turn| turn.turn_number));
            }
        }
        assert_eq!(received, [0, 1, 2]);
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), subscription.updates.recv())
                .await
                .unwrap()
                .unwrap(),
            WatchedGameUpdate::Terminal(GameStatus::Finished)
        ));
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }
}
