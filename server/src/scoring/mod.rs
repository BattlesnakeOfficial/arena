use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

/// Event representing the results of a completed leaderboard game.
/// Passed to each scoring algorithm to update its internal state.
pub struct GameResultEvent {
    pub leaderboard_game_id: Uuid,
    pub leaderboard_id: Uuid,
    pub game_id: Uuid,
    /// (leaderboard_entry_id, battlesnake_id, placement). Placement is 1-indexed (1 = winner).
    pub results: Vec<GameResultEntry>,
}

pub struct GameResultEntry {
    pub leaderboard_entry_id: Uuid,
    pub battlesnake_id: Uuid,
    pub placement: i32,
    /// Current mu from the locked leaderboard_entries row.
    /// Algorithms can use this as a fallback instead of re-querying.
    pub mu: f64,
    /// Current sigma from the locked leaderboard_entries row.
    pub sigma: f64,
    /// The game_battlesnake_id for this entry's participation in the game.
    /// Matches the "ID" field in frame data (as a UUID string).
    pub game_battlesnake_id: Uuid,
}

/// A score for a single leaderboard entry, as computed by a scoring algorithm.
pub struct EntryScore {
    pub leaderboard_entry_id: Uuid,
    pub score: f64,
    /// Extra display columns, e.g. ("mu", "25.0"), ("wins", "3")
    pub details: Vec<(String, String)>,
}

/// Trait for pluggable scoring algorithms.
/// Each algorithm independently computes scores for leaderboard entries.
#[async_trait]
pub trait ScoringAlgorithm: Send + Sync {
    /// Unique stable key identifying this algorithm (e.g. "elo", "win_rate", "weng_lin").
    fn key(&self) -> &'static str;

    /// Human-readable display name (e.g. "Weng-Lin", "Win Rate").
    fn display_name(&self) -> &'static str;

    /// Column header for the score in rankings tables (e.g. "Rating", "Win %").
    fn score_column_name(&self) -> &'static str;

    /// Initialize state for a new leaderboard entry.
    /// Must use INSERT ... ON CONFLICT DO NOTHING for idempotency.
    async fn initialize_entry(&self, pool: &PgPool, leaderboard_entry_id: Uuid) -> cja::Result<()>;

    /// Process a completed game. Called within a transaction.
    async fn process_game_result(
        &self,
        conn: &mut sqlx::PgConnection,
        event: &GameResultEvent,
    ) -> cja::Result<()>;

    /// Batch fetch scores for the given entry IDs.
    /// Callers are responsible for pagination/filtering — this just looks up scores
    /// for the provided IDs. Returns results in no guaranteed order.
    async fn get_scores(&self, pool: &PgPool, entry_ids: &[Uuid]) -> cja::Result<Vec<EntryScore>>;

    /// Fetch score for a single entry.
    async fn get_entry_score(
        &self,
        pool: &PgPool,
        leaderboard_entry_id: Uuid,
    ) -> cja::Result<Option<EntryScore>>;
}

/// Registry of scoring algorithms. All leaderboards use all registered algorithms.
pub struct ScoringRegistry {
    algorithms: Vec<Box<dyn ScoringAlgorithm>>,
}

impl ScoringRegistry {
    pub fn new() -> Self {
        Self { algorithms: vec![] }
    }

    pub fn register(&mut self, algo: Box<dyn ScoringAlgorithm>) {
        self.algorithms.push(algo);
    }

    pub fn algorithms(&self) -> &[Box<dyn ScoringAlgorithm>] {
        &self.algorithms
    }

    pub fn get(&self, key: &str) -> Option<&dyn ScoringAlgorithm> {
        self.algorithms
            .iter()
            .find(|a| a.key() == key)
            .map(|a| a.as_ref())
    }
}

pub mod food_eaten;
pub mod weng_lin;
pub mod win_rate;

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal mock scoring algorithm for testing the registry.
    struct MockAlgorithm {
        key: &'static str,
        display_name: &'static str,
    }

    #[async_trait]
    impl ScoringAlgorithm for MockAlgorithm {
        fn key(&self) -> &'static str {
            self.key
        }

        fn display_name(&self) -> &'static str {
            self.display_name
        }

        fn score_column_name(&self) -> &'static str {
            "Score"
        }

        async fn initialize_entry(
            &self,
            _pool: &PgPool,
            _leaderboard_entry_id: Uuid,
        ) -> cja::Result<()> {
            Ok(())
        }

        async fn process_game_result(
            &self,
            _conn: &mut sqlx::PgConnection,
            _event: &GameResultEvent,
        ) -> cja::Result<()> {
            Ok(())
        }

        async fn get_scores(
            &self,
            _pool: &PgPool,
            _entry_ids: &[Uuid],
        ) -> cja::Result<Vec<EntryScore>> {
            Ok(vec![])
        }

        async fn get_entry_score(
            &self,
            _pool: &PgPool,
            _leaderboard_entry_id: Uuid,
        ) -> cja::Result<Option<EntryScore>> {
            Ok(None)
        }
    }

    #[test]
    fn test_registry_new_is_empty() {
        let registry = ScoringRegistry::new();
        assert!(
            registry.algorithms().is_empty(),
            "New registry should have no algorithms"
        );
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = ScoringRegistry::new();
        registry.register(Box::new(MockAlgorithm {
            key: "mock_a",
            display_name: "Mock A",
        }));
        registry.register(Box::new(MockAlgorithm {
            key: "mock_b",
            display_name: "Mock B",
        }));

        assert_eq!(
            registry.algorithms().len(),
            2,
            "Registry should have 2 algorithms after registering 2"
        );

        let algo_a = registry
            .get("mock_a")
            .expect("Should find algorithm with key 'mock_a'");
        assert_eq!(algo_a.key(), "mock_a");
        assert_eq!(algo_a.display_name(), "Mock A");

        let algo_b = registry
            .get("mock_b")
            .expect("Should find algorithm with key 'mock_b'");
        assert_eq!(algo_b.key(), "mock_b");
        assert_eq!(algo_b.display_name(), "Mock B");
    }

    #[test]
    fn test_registry_get_nonexistent_returns_none() {
        let registry = ScoringRegistry::new();
        assert!(
            registry.get("nonexistent").is_none(),
            "Looking up a nonexistent key should return None"
        );
    }

    #[test]
    fn test_registry_get_with_registered_algorithms_nonexistent_still_none() {
        let mut registry = ScoringRegistry::new();
        registry.register(Box::new(MockAlgorithm {
            key: "elo",
            display_name: "ELO",
        }));

        assert!(
            registry.get("win_rate").is_none(),
            "Looking up an unregistered key should return None even when other keys exist"
        );
    }

    #[test]
    fn test_registry_with_real_algorithms() {
        let mut registry = ScoringRegistry::new();
        registry.register(Box::new(weng_lin::WengLinScoring));
        registry.register(Box::new(win_rate::WinRateScoring));
        assert_eq!(registry.algorithms().len(), 2);
        assert_eq!(registry.get("weng_lin").unwrap().key(), "weng_lin");
        assert_eq!(registry.get("weng_lin").unwrap().display_name(), "Weng-Lin");
        assert_eq!(
            registry.get("weng_lin").unwrap().score_column_name(),
            "Rating"
        );
        assert_eq!(registry.get("win_rate").unwrap().key(), "win_rate");
        assert_eq!(registry.get("win_rate").unwrap().display_name(), "Win Rate");
        assert_eq!(
            registry.get("win_rate").unwrap().score_column_name(),
            "Win %"
        );
    }

    #[test]
    fn test_game_result_event_construction() {
        let event = GameResultEvent {
            leaderboard_game_id: Uuid::new_v4(),
            leaderboard_id: Uuid::new_v4(),
            game_id: Uuid::new_v4(),
            results: vec![
                GameResultEntry {
                    leaderboard_entry_id: Uuid::new_v4(),
                    battlesnake_id: Uuid::new_v4(),
                    placement: 1,
                    mu: 25.0,
                    sigma: 8.333,
                    game_battlesnake_id: Uuid::new_v4(),
                },
                GameResultEntry {
                    leaderboard_entry_id: Uuid::new_v4(),
                    battlesnake_id: Uuid::new_v4(),
                    placement: 2,
                    mu: 25.0,
                    sigma: 8.333,
                    game_battlesnake_id: Uuid::new_v4(),
                },
            ],
        };

        assert_eq!(event.results.len(), 2);
        assert_eq!(
            event.results[0].placement, 1,
            "First entry should be winner"
        );
        assert_eq!(
            event.results[1].placement, 2,
            "Second entry should be runner-up"
        );
    }

    #[test]
    fn test_entry_score_details() {
        let score = EntryScore {
            leaderboard_entry_id: Uuid::new_v4(),
            score: 42.5,
            details: vec![
                ("mu".to_string(), "25.0".to_string()),
                ("sigma".to_string(), "5.0".to_string()),
            ],
        };

        assert_eq!(score.score, 42.5);
        assert_eq!(score.details.len(), 2);
        assert_eq!(score.details[0].0, "mu");
        assert_eq!(score.details[0].1, "25.0");
    }
}
