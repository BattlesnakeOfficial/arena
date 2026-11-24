SET LOCAL lock_timeout = '5s';
ALTER TABLE leaderboard_entries VALIDATE CONSTRAINT leaderboard_entries_season_required;
ALTER TABLE leaderboard_games VALIDATE CONSTRAINT leaderboard_games_season_required;
ALTER TABLE leaderboard_entries VALIDATE CONSTRAINT leaderboard_entries_season_league_fk;
ALTER TABLE leaderboard_games VALIDATE CONSTRAINT leaderboard_games_season_league_fk;
DO $$
BEGIN
    IF (SELECT count(*) FROM pg_index WHERE indexrelid IN (
        'leaderboard_entries_season_snake_idx'::regclass,
        'leaderboard_games_season_created_idx'::regclass
    ) AND indisvalid AND indisready) <> 2 THEN
        RAISE EXCEPTION 'season indexes are missing or invalid';
    END IF;
    IF EXISTS (SELECT 1 FROM leaderboard_entries WHERE season_id IS NULL)
       OR EXISTS (SELECT 1 FROM leaderboard_games WHERE season_id IS NULL)
       OR EXISTS (SELECT 1 FROM leaderboard_entries e JOIN leaderboard_seasons s USING (season_id)
                  WHERE e.leaderboard_id <> s.leaderboard_id)
       OR EXISTS (SELECT 1 FROM leaderboard_games g JOIN leaderboard_seasons s USING (season_id)
                  WHERE g.leaderboard_id <> s.leaderboard_id)
       OR EXISTS (SELECT 1 FROM leaderboard_game_results r
                  JOIN leaderboard_games g USING (leaderboard_game_id)
                  JOIN leaderboard_entries e USING (leaderboard_entry_id)
                  WHERE g.season_id <> e.season_id) THEN
        RAISE EXCEPTION 'leaderboard season links are incomplete or inconsistent';
    END IF;
END;
$$;
