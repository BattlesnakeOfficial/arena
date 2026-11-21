SET LOCAL lock_timeout = '5s';

CREATE PROCEDURE backfill_leaderboard_seasons() LANGUAGE plpgsql AS $$
DECLARE
    last_entry UUID := '00000000-0000-0000-0000-000000000000';
    last_game UUID := '00000000-0000-0000-0000-000000000000';
    batch_end UUID;
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    LOOP
        SELECT leaderboard_entry_id INTO batch_end FROM (
            SELECT leaderboard_entry_id FROM leaderboard_entries
            WHERE season_id IS NULL AND leaderboard_entry_id > last_entry
            ORDER BY leaderboard_entry_id LIMIT 10000
        ) batch ORDER BY leaderboard_entry_id DESC LIMIT 1;
        EXIT WHEN batch_end IS NULL;
        IF EXISTS (
            SELECT 1 FROM leaderboard_entries e
            WHERE e.season_id IS NULL AND e.leaderboard_entry_id > last_entry
              AND e.leaderboard_entry_id <= batch_end
              AND NOT EXISTS (SELECT 1 FROM leaderboard_seasons s
                              WHERE s.leaderboard_id = e.leaderboard_id AND s.name = 'Preseason')
        ) THEN
            RAISE EXCEPTION 'entry missing Preseason';
        END IF;
        UPDATE leaderboard_entries e SET season_id = s.season_id
        FROM leaderboard_seasons s
        WHERE e.season_id IS NULL AND e.leaderboard_entry_id > last_entry
          AND e.leaderboard_entry_id <= batch_end
          AND s.leaderboard_id = e.leaderboard_id AND s.name = 'Preseason';
        last_entry := batch_end;
        COMMIT;
        PERFORM set_config('lock_timeout', '5s', true);
    END LOOP;
    LOOP
        SELECT leaderboard_game_id INTO batch_end FROM (
            SELECT leaderboard_game_id FROM leaderboard_games
            WHERE season_id IS NULL AND leaderboard_game_id > last_game
            ORDER BY leaderboard_game_id LIMIT 10000
        ) batch ORDER BY leaderboard_game_id DESC LIMIT 1;
        EXIT WHEN batch_end IS NULL;
        IF EXISTS (
            SELECT 1 FROM leaderboard_games g
            WHERE g.season_id IS NULL AND g.leaderboard_game_id > last_game
              AND g.leaderboard_game_id <= batch_end
              AND NOT EXISTS (SELECT 1 FROM leaderboard_seasons s
                              WHERE s.leaderboard_id = g.leaderboard_id AND s.name = 'Preseason')
        ) THEN
            RAISE EXCEPTION 'game missing Preseason';
        END IF;
        UPDATE leaderboard_games g SET season_id = s.season_id
        FROM leaderboard_seasons s
        WHERE g.season_id IS NULL AND g.leaderboard_game_id > last_game
          AND g.leaderboard_game_id <= batch_end
          AND s.leaderboard_id = g.leaderboard_id AND s.name = 'Preseason';
        last_game := batch_end;
        COMMIT;
        PERFORM set_config('lock_timeout', '5s', true);
    END LOOP;
END;
$$;
