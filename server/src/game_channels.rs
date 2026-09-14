use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

/// Notification sent when a turn completes
#[derive(Debug, Clone)]
pub struct TurnNotification {
    pub game_id: Uuid,
    pub turn_number: i32,
}

/// Manages broadcast channels for live game updates
/// One broadcast channel per active game, subscribers receive turn notifications
#[derive(Debug, Clone)]
pub struct GameChannels {
    /// Map from game_id to broadcast sender for that game
    channels: Arc<RwLock<HashMap<Uuid, broadcast::Sender<TurnNotification>>>>,
}

impl Default for GameChannels {
    fn default() -> Self {
        Self::new()
    }
}

impl GameChannels {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a broadcast channel for a game
    /// Returns a receiver that will receive turn notifications
    pub async fn subscribe(&self, game_id: Uuid) -> broadcast::Receiver<TurnNotification> {
        let mut channels = self.channels.write().await;

        if let Some(sender) = channels.get(&game_id) {
            sender.subscribe()
        } else {
            // Create new channel with buffer of 256 turns
            // This should be enough to handle temporary slowdowns
            let (sender, receiver) = broadcast::channel(256);
            channels.insert(game_id, sender);
            receiver
        }
    }

    /// Send a turn notification to all subscribers for a game
    pub async fn notify(&self, notification: TurnNotification) {
        let channels = self.channels.read().await;

        if let Some(sender) = channels.get(&notification.game_id) {
            // Ignore errors - they mean no receivers are listening
            let _ = sender.send(notification);
        }
    }

    /// Clean up a game's channel if no receivers are listening
    /// Call this periodically or when a game ends
    pub async fn cleanup(&self, game_id: Uuid) {
        let mut channels = self.channels.write().await;

        if let Some(sender) = channels.get(&game_id)
            && sender.receiver_count() == 0
        {
            channels.remove(&game_id);
            tracing::debug!(game_id = %game_id, "Removed game channel (no subscribers)");
        }
    }

    /// Remove a game's channel entirely (call when game ends)
    pub async fn remove(&self, game_id: Uuid) {
        let mut channels = self.channels.write().await;
        channels.remove(&game_id);
        tracing::debug!(game_id = %game_id, "Removed game channel");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    async fn blocked_notification_releases_the_turn_database_connection(database: sqlx::PgPool) {
        let game_id = sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(&database).await.unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(database.connect_options().as_ref().clone())
            .await
            .unwrap();
        let channels = GameChannels::new();
        let mut receiver = channels.subscribe(game_id).await;
        let map = channels.channels.write().await;
        let work = crate::models::turn::create_turn(&pool, &channels, game_id, 0, None);
        tokio::pin!(work);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut work => panic!("notification must wait for channel map: {result:?}"),
                () = async {
                    while sqlx::query_scalar!("SELECT count(*) FROM turns WHERE game_id=$1", game_id)
                        .fetch_one(&database).await.unwrap() == Some(0) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                } => {}
            }
            tokio::select! {
                result = &mut work => panic!("notification must still be blocked: {result:?}"),
                connection = pool.acquire() => { drop(connection.unwrap()); }
            }
        }).await.unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(map);
        work.await.unwrap();
        assert_eq!(receiver.try_recv().unwrap().turn_number, 0);
    }

    #[tokio::test]
    async fn test_subscribe_creates_channel() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        let _receiver = channels.subscribe(game_id).await;

        // Channel should exist now
        assert!(channels.channels.read().await.contains_key(&game_id));
    }

    #[tokio::test]
    async fn test_notify_sends_to_subscribers() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        let mut receiver = channels.subscribe(game_id).await;

        channels
            .notify(TurnNotification {
                game_id,
                turn_number: 5,
            })
            .await;

        let notification = receiver.recv().await.unwrap();
        assert_eq!(notification.game_id, game_id);
        assert_eq!(notification.turn_number, 5);
    }

    #[tokio::test]
    async fn test_cleanup_removes_empty_channels() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        // Create and then drop a subscriber
        {
            let _receiver = channels.subscribe(game_id).await;
        }

        // Channel should still exist but with no receivers
        channels.cleanup(game_id).await;

        // Channel should be removed
        assert!(!channels.channels.read().await.contains_key(&game_id));
    }

    #[tokio::test]
    async fn test_multiple_turn_notifications() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        let mut receiver = channels.subscribe(game_id).await;

        // Send multiple turn notifications (simulating game progression)
        for turn in 0..10 {
            channels
                .notify(TurnNotification {
                    game_id,
                    turn_number: turn,
                })
                .await;
        }

        // Verify all turns are received in order
        for expected_turn in 0..10 {
            let notification = receiver.recv().await.unwrap();
            assert_eq!(notification.turn_number, expected_turn);
            assert_eq!(notification.game_id, game_id);
        }
    }

    #[tokio::test]
    async fn test_multiple_games_isolated() {
        let channels = GameChannels::new();
        let game_1 = Uuid::new_v4();
        let game_2 = Uuid::new_v4();

        let mut receiver_1 = channels.subscribe(game_1).await;
        let mut receiver_2 = channels.subscribe(game_2).await;

        // Notify different games
        channels
            .notify(TurnNotification {
                game_id: game_1,
                turn_number: 1,
            })
            .await;
        channels
            .notify(TurnNotification {
                game_id: game_2,
                turn_number: 100,
            })
            .await;

        // Each receiver only gets its game's notifications
        let notif_1 = receiver_1.recv().await.unwrap();
        assert_eq!(notif_1.game_id, game_1);
        assert_eq!(notif_1.turn_number, 1);

        let notif_2 = receiver_2.recv().await.unwrap();
        assert_eq!(notif_2.game_id, game_2);
        assert_eq!(notif_2.turn_number, 100);
    }

    #[tokio::test]
    async fn test_notify_without_subscribers() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        // Should not panic when notifying with no subscribers
        channels
            .notify(TurnNotification {
                game_id,
                turn_number: 5,
            })
            .await;
    }

    #[tokio::test]
    async fn test_multiple_subscribers_same_game() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        let mut receiver_1 = channels.subscribe(game_id).await;
        let mut receiver_2 = channels.subscribe(game_id).await;

        channels
            .notify(TurnNotification {
                game_id,
                turn_number: 42,
            })
            .await;

        // Both subscribers should receive the notification
        let notif_1 = receiver_1.recv().await.unwrap();
        let notif_2 = receiver_2.recv().await.unwrap();

        assert_eq!(notif_1.turn_number, 42);
        assert_eq!(notif_2.turn_number, 42);
    }

    #[tokio::test]
    async fn test_remove_channel() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        let _receiver = channels.subscribe(game_id).await;
        assert!(channels.channels.read().await.contains_key(&game_id));

        channels.remove(game_id).await;
        assert!(!channels.channels.read().await.contains_key(&game_id));
    }

    #[tokio::test]
    async fn test_cleanup_preserves_active_channels() {
        let channels = GameChannels::new();
        let game_id = Uuid::new_v4();

        // Keep the receiver alive
        let _receiver = channels.subscribe(game_id).await;

        // Cleanup should NOT remove the channel since there's an active subscriber
        channels.cleanup(game_id).await;

        // Channel should still exist
        assert!(channels.channels.read().await.contains_key(&game_id));
    }

    #[test]
    fn test_turn_notification_clone() {
        let notification = TurnNotification {
            game_id: Uuid::new_v4(),
            turn_number: 10,
        };

        let cloned = notification.clone();
        assert_eq!(notification.game_id, cloned.game_id);
        assert_eq!(notification.turn_number, cloned.turn_number);
    }

    #[test]
    fn test_game_channels_default() {
        let channels = GameChannels::default();
        // Should be equivalent to new()
        assert!(channels.channels.try_read().is_ok());
    }
}
