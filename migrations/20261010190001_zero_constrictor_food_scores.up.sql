-- Food eaten used to be derived from body growth, but Constrictor grows every
-- snake every turn and never spawns food, so its "food" was turns survived.
-- No Constrictor snake has ever eaten, so the true score is zero.
UPDATE food_eaten_stats fes
SET food_score = 0, updated_at = NOW()
FROM leaderboard_entries le
JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
WHERE fes.leaderboard_entry_id = le.leaderboard_entry_id
  AND l.game_type = 'Constrictor'
  AND fes.food_score <> 0;

UPDATE leaderboard_game_results lgr
SET food_eaten = 0
FROM leaderboard_entries le
JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
WHERE lgr.leaderboard_entry_id = le.leaderboard_entry_id
  AND l.game_type = 'Constrictor'
  AND lgr.food_eaten <> 0;
