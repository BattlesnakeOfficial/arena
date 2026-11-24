SET LOCAL lock_timeout = '5s';

CREATE TRIGGER fill_leaderboard_entry_season_before_insert
    BEFORE INSERT ON leaderboard_entries FOR EACH ROW EXECUTE FUNCTION fill_leaderboard_season_id();

CREATE FUNCTION sync_entry_state_to_registration() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO leaderboard_registrations (leaderboard_id, battlesnake_id, disabled_at, disabled_reason,
            health_consecutive_failures, health_consecutive_successes, health_last_checked_at, health_last_failure)
        VALUES (NEW.leaderboard_id, NEW.battlesnake_id, NEW.disabled_at, NEW.disabled_reason,
            NEW.health_consecutive_failures, NEW.health_consecutive_successes, NEW.health_last_checked_at, NEW.health_last_failure)
        ON CONFLICT (leaderboard_id, battlesnake_id) DO NOTHING;
    ELSE
        INSERT INTO leaderboard_registrations (leaderboard_id, battlesnake_id, disabled_at, disabled_reason,
            health_consecutive_failures, health_consecutive_successes, health_last_checked_at, health_last_failure)
        VALUES (NEW.leaderboard_id, NEW.battlesnake_id, NEW.disabled_at, NEW.disabled_reason,
            NEW.health_consecutive_failures, NEW.health_consecutive_successes, NEW.health_last_checked_at, NEW.health_last_failure)
        ON CONFLICT (leaderboard_id, battlesnake_id) DO UPDATE SET
            disabled_at = EXCLUDED.disabled_at,
            disabled_reason = EXCLUDED.disabled_reason,
            health_consecutive_failures = EXCLUDED.health_consecutive_failures,
            health_consecutive_successes = EXCLUDED.health_consecutive_successes,
            health_last_checked_at = EXCLUDED.health_last_checked_at,
            health_last_failure = EXCLUDED.health_last_failure,
            updated_at = NOW();
        -- Deliberately NO terminal-reason guard: it would block old-revision
        -- unretire_battlesnake from clearing 'retired'.
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER sync_entry_state_to_registration
    AFTER INSERT OR UPDATE OF disabled_at, disabled_reason, health_consecutive_failures,
        health_consecutive_successes, health_last_checked_at, health_last_failure
    ON leaderboard_entries
    FOR EACH ROW EXECUTE FUNCTION sync_entry_state_to_registration();
