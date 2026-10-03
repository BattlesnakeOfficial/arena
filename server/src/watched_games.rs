#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use tokio::sync::{Mutex, Notify, broadcast, watch};
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
}

#[derive(Default)]
struct Registry {
    entries: HashMap<Uuid, Arc<WatchEntry>>,
    dirty: HashSet<Uuid>,
}

#[derive(Clone, Default)]
pub struct WatchedGames {
    registry: Arc<Mutex<Registry>>,
    wake: Arc<Notify>,
    #[cfg(test)]
    stats: Arc<PollStats>,
}

#[cfg(test)]
#[derive(Default)]
struct PollStats {
    status_reads: AtomicUsize,
    range_reads: AtomicUsize,
    batches: Mutex<Vec<(tokio::time::Instant, Vec<Uuid>)>>,
    local_starts: Mutex<Vec<tokio::time::Instant>>,
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
        let watched = self.watched.clone();
        let game_id = self.game_id;
        let entry = self.entry.clone();
        tokio::spawn(async move { watched.retire_pending(game_id, &entry).await });
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
        let entry = Arc::new(WatchEntry { updates, readiness });
        registry.entries.insert(game_id, entry.clone());
        registry.dirty.remove(&game_id);
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

    pub async fn seed_if_current(
        &self,
        game_id: Uuid,
        subscription: &mut Subscription,
        cursor: i32,
    ) {
        let registry = self.registry.lock().await;
        if registry
            .entries
            .get(&game_id)
            .is_some_and(|entry| Arc::ptr_eq(entry, &subscription.entry))
            && *subscription.entry.readiness.borrow() == Readiness::Pending
        {
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

    async fn retire_pending(&self, game_id: Uuid, entry: &Arc<WatchEntry>) {
        let mut registry = self.registry.lock().await;
        if registry
            .entries
            .get(&game_id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
            && *entry.readiness.borrow() == Readiness::Pending
        {
            entry.readiness.send_replace(Readiness::Retired);
            registry.entries.remove(&game_id);
            registry.dirty.remove(&game_id);
        }
    }

    pub async fn turn_persisted(&self, game_id: Uuid) {
        let mut registry = self.registry.lock().await;
        if registry.entries.get(&game_id).is_some_and(|entry| {
            matches!(*entry.readiness.borrow(), Readiness::Active { .. })
                && entry.updates.receiver_count() > 0
        }) && registry.dirty.insert(game_id)
        {
            self.wake.notify_one();
        }
    }

    async fn snapshot(&self, full: bool) -> Vec<(Uuid, Arc<WatchEntry>, i32)> {
        let mut registry = self.registry.lock().await;
        registry
            .entries
            .retain(|_, entry| entry.updates.receiver_count() > 0);
        let active_ids: HashSet<_> = registry.entries.keys().copied().collect();
        registry.dirty.retain(|id| active_ids.contains(id));
        let selected = if full {
            let ids: Vec<_> = registry.entries.keys().copied().collect();
            for id in ids {
                registry.dirty.remove(&id);
            }
            registry
                .entries
                .iter()
                .map(|(id, entry)| (*id, entry.clone()))
                .collect::<Vec<_>>()
        } else {
            let ids = std::mem::take(&mut registry.dirty);
            ids.into_iter()
                .filter_map(|id| registry.entries.get(&id).map(|entry| (id, entry.clone())))
                .collect()
        };
        selected
            .into_iter()
            .filter_map(|(id, entry)| {
                let Readiness::Active { cursor } = *entry.readiness.borrow() else {
                    return None;
                };
                Some((id, entry, cursor))
            })
            .collect()
    }
}

// Each pass makes two queries regardless of the number of local viewers.
async fn poll_batch(
    pool: &PgPool,
    watched: &WatchedGames,
    snapshot: Vec<(Uuid, Arc<WatchEntry>, i32)>,
) -> cja::Result<()> {
    if snapshot.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = snapshot.iter().map(|(id, _, _)| *id).collect();
    let cursors: Vec<i32> = snapshot.iter().map(|(_, _, cursor)| *cursor).collect();
    #[cfg(test)]
    {
        watched.stats.status_reads.fetch_add(1, Ordering::Relaxed);
        watched
            .stats
            .batches
            .lock()
            .await
            .push((tokio::time::Instant::now(), ids.clone()));
    }
    let statuses = sqlx::query!(
        r#"SELECT g.game_id, g.status, (SELECT MAX(t.turn_number) FROM turns t WHERE t.game_id = g.game_id) AS latest_turn
           FROM games g WHERE g.game_id = ANY($1::uuid[])"#,
        &ids
    ).fetch_all(pool).await.wrap_err("Failed to read watched game statuses")?;
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
        grouped.entry(turn.game_id).or_default().push(turn);
    }
    let status_map: HashMap<_, _> = statuses.into_iter().map(|row| (row.game_id, row)).collect();

    let mut registry = watched.registry.lock().await;
    for (id, entry, cursor) in snapshot {
        if !registry
            .entries
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &entry))
            || *entry.readiness.borrow() != (Readiness::Active { cursor })
        {
            continue;
        }
        let Some(row) = status_map.get(&id) else {
            let _ = entry.updates.send(WatchedGameUpdate::Missing);
            entry.readiness.send_replace(Readiness::Retired);
            registry.entries.remove(&id);
            continue;
        };
        if row.latest_turn.unwrap_or(-1) < cursor {
            let _ = entry.updates.send(WatchedGameUpdate::Reset);
            entry.readiness.send_replace(Readiness::Retired);
            registry.entries.remove(&id);
            continue;
        }
        let mut next_cursor = cursor;
        if let Some(frames) = grouped.remove(&id) {
            if let Some(last) = frames.last() {
                next_cursor = last.turn_number;
            }
            let _ = entry
                .updates
                .send(WatchedGameUpdate::Frames(Arc::from(frames)));
            entry.readiness.send_replace(Readiness::Active {
                cursor: next_cursor,
            });
        }
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

pub(crate) async fn run_watched_games(
    pool: PgPool,
    watched: WatchedGames,
    shutdown: CancellationToken,
) -> cja::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_local = tokio::time::Instant::now();
    let mut pending_local = false;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {
                let snapshot = watched.snapshot(true).await;
                if let Err(error) = poll_batch(&pool, &watched, snapshot).await {
                    tracing::error!(error = %format_args!("{error:#}"), "Watched game sweep failed");
                }
            }
            () = watched.wake.notified() => {
                pending_local = true;
            }
            () = tokio::time::sleep_until(next_local), if pending_local => {
                pending_local = false;
                next_local = tokio::time::Instant::now() + Duration::from_millis(25);
                #[cfg(test)]
                watched.stats.local_starts.lock().await.push(tokio::time::Instant::now());
                let snapshot = watched.snapshot(false).await;
                if let Err(error) = poll_batch(&pool, &watched, snapshot).await {
                    tracing::error!(error = %format_args!("{error:#}"), "Watched game local update failed");
                }
            }
        }
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
        watched.turn_persisted(id).await;
        assert_eq!(watched.registry.lock().await.dirty.len(), 1);
        watched.turn_persisted(Uuid::new_v4()).await;
        assert_eq!(watched.registry.lock().await.dirty.len(), 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn blocked_notification_releases_turn_connection(database: PgPool) {
        let id = sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(&database).await.unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(database.connect_options().as_ref().clone())
            .await
            .unwrap();
        let watched = WatchedGames::new();
        let mut subscriber = watched.subscribe(id).await;
        watched.seed_if_current(id, &mut subscriber, -1).await;
        let guard = watched.registry.lock().await;
        let work = crate::models::turn::create_turn(&pool, &watched, id, 0, None);
        tokio::pin!(work);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut work => panic!("notification must wait for registry: {result:?}"),
                () = async {
                    while sqlx::query_scalar!("SELECT count(*) FROM turns WHERE game_id=$1", id)
                        .fetch_one(&database).await.unwrap() == Some(0) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                } => {}
            }
            tokio::select! {
                result = &mut work => panic!("notification must still be blocked: {result:?}"),
                connection = pool.acquire() => { drop(connection.unwrap()); }
            }
        })
        .await
        .unwrap();
        drop(guard);
        work.await.unwrap();
        assert!(watched.registry.lock().await.dirty.contains(&id));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn remote_poller_catches_up_and_reports_terminal(pool: PgPool) {
        let id = sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(&pool).await.unwrap();
        let writer = WatchedGames::new();
        let reader = WatchedGames::new();
        let mut subscription = reader.subscribe(id).await;
        reader.seed_if_current(id, &mut subscription, -1).await;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_watched_games(
            pool.clone(),
            reader.clone(),
            shutdown.clone(),
        ));
        for turn in 0..3 {
            crate::models::turn::create_turn(
                &pool,
                &writer,
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

#[cfg(test)]
mod batch_tests {
    use super::*;

    async fn game(pool: &PgPool) -> Uuid {
        sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(pool).await.unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn query_work_is_per_watched_game_not_viewer(pool: PgPool) {
        let watched = WatchedGames::new();
        let mut subscriptions = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..50 {
            let id = game(&pool).await;
            let mut creator = watched.subscribe(id).await;
            watched.seed_if_current(id, &mut creator, -1).await;
            subscriptions.push(creator);
            for _ in 0..9 {
                subscriptions.push(watched.subscribe(id).await);
            }
            ids.push(id);
        }
        assert_eq!(subscriptions.len(), 500);
        poll_batch(&pool, &watched, watched.snapshot(true).await)
            .await
            .unwrap();
        assert_eq!(watched.stats.status_reads.load(Ordering::Relaxed), 1);
        assert_eq!(watched.stats.range_reads.load(Ordering::Relaxed), 1);
        assert_eq!(watched.stats.batches.lock().await[0].1.len(), 50);

        for _ in 0..10 {
            let unwatched = game(&pool).await;
            crate::models::turn::create_turn(
                &pool,
                &watched,
                unwatched,
                0,
                Some(serde_json::json!({"Turn":0})),
            )
            .await
            .unwrap();
        }
        assert!(watched.registry.lock().await.dirty.is_empty());
        assert!(watched.snapshot(false).await.is_empty());
        assert_eq!(watched.stats.status_reads.load(Ordering::Relaxed), 1);
        crate::models::turn::create_turn(
            &pool,
            &watched,
            ids[0],
            0,
            Some(serde_json::json!({"Turn":0})),
        )
        .await
        .unwrap();
        poll_batch(&pool, &watched, watched.snapshot(false).await)
            .await
            .unwrap();
        assert_eq!(watched.stats.status_reads.load(Ordering::Relaxed), 2);
        assert_eq!(watched.stats.range_reads.load(Ordering::Relaxed), 2);
        assert_eq!(watched.stats.batches.lock().await[1].1, vec![ids[0]]);
        let mut shared = None;
        for subscriber in &mut subscriptions[..10] {
            let WatchedGameUpdate::Frames(frames) = subscriber.updates.recv().await.unwrap() else {
                panic!("expected shared frame batch");
            };
            if let Some(previous) = &shared {
                assert!(Arc::ptr_eq(previous, &frames));
            } else {
                shared = Some(frames);
            }
        }
        drop(subscriptions);
        poll_batch(&pool, &watched, watched.snapshot(true).await)
            .await
            .unwrap();
        assert_eq!(watched.stats.status_reads.load(Ordering::Relaxed), 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn retry_rewind_retires_old_entry_and_starts_at_zero(pool: PgPool) {
        let id = game(&pool).await;
        let watched = WatchedGames::new();
        let mut original = watched.subscribe(id).await;
        watched.seed_if_current(id, &mut original, -1).await;
        for number in 0..3 {
            crate::models::turn::create_turn(
                &pool,
                &watched,
                id,
                number,
                Some(serde_json::json!({"Turn":number})),
            )
            .await
            .unwrap();
        }
        poll_batch(&pool, &watched, watched.snapshot(true).await)
            .await
            .unwrap();
        crate::models::game::reset_game_state_for_retry(&pool, id)
            .await
            .unwrap();
        poll_batch(&pool, &watched, watched.snapshot(true).await)
            .await
            .unwrap();
        assert!(matches!(
            original.updates.recv().await.unwrap(),
            WatchedGameUpdate::Frames(_)
        ));
        assert!(matches!(
            original.updates.recv().await.unwrap(),
            WatchedGameUpdate::Reset
        ));
        let mut replacement = watched.subscribe(id).await;
        assert!(replacement.created);
        assert!(!Arc::ptr_eq(&original.entry, &replacement.entry));
        watched.seed_if_current(id, &mut replacement, -1).await;
        crate::models::turn::create_turn(
            &pool,
            &watched,
            id,
            0,
            Some(serde_json::json!({"Turn":0})),
        )
        .await
        .unwrap();
        poll_batch(&pool, &watched, watched.snapshot(true).await)
            .await
            .unwrap();
        let WatchedGameUpdate::Frames(frames) = replacement.updates.recv().await.unwrap() else {
            panic!("expected frames")
        };
        assert_eq!(frames[0].turn_number, 0);
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn failed_query_retries_without_losing_frames(pool: PgPool) {
        let id = game(&pool).await;
        let watched = WatchedGames::new();
        let mut subscriber = watched.subscribe(id).await;
        watched.seed_if_current(id, &mut subscriber, -1).await;
        sqlx::query!("ALTER TABLE turns RENAME TO turns_hidden")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            poll_batch(&pool, &watched, watched.snapshot(true).await)
                .await
                .is_err()
        );
        assert_eq!(
            *subscriber.readiness.borrow(),
            Readiness::Active { cursor: -1 }
        );
        sqlx::query!("ALTER TABLE turns_hidden RENAME TO turns")
            .execute(&pool)
            .await
            .unwrap();
        crate::models::turn::create_turn(
            &pool,
            &WatchedGames::new(),
            id,
            0,
            Some(serde_json::json!({"Turn":0})),
        )
        .await
        .unwrap();
        poll_batch(&pool, &watched, watched.snapshot(true).await)
            .await
            .unwrap();
        let WatchedGameUpdate::Frames(frames) = subscriber.updates.recv().await.unwrap() else {
            panic!("expected catch-up frame")
        };
        assert_eq!(frames[0].turn_number, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn local_batch_starts_are_spaced(pool: PgPool) {
        let id = game(&pool).await;
        let watched = WatchedGames::new();
        let mut subscriber = watched.subscribe(id).await;
        watched.seed_if_current(id, &mut subscriber, -1).await;
        let shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            watched.clone(),
            shutdown.clone(),
        ));
        for number in 0..10 {
            crate::models::turn::create_turn(
                &pool,
                &watched,
                id,
                number,
                Some(serde_json::json!({"Turn":number})),
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();
        poller.await.unwrap().unwrap();
        let starts = watched.stats.local_starts.lock().await;
        assert!(!starts.is_empty());
        assert!(
            starts
                .windows(2)
                .all(|pair| pair[1].duration_since(pair[0]) >= Duration::from_millis(25))
        );
    }
}
