-- DEV-1984: resumable play-history archive walk state.
CREATE TABLE play_history_walk_cursor (
    singleton BOOLEAN PRIMARY KEY NOT NULL DEFAULT TRUE CHECK (singleton),
    -- Engine games.created is TIMESTAMP (no tz), stored/read as UTC.
    -- Seeded with a FINITE sentinel that predates all Play data (oldest
    -- engine game: 2019-11). Do NOT use '-infinity': Postgres sends it
    -- as i64::MIN over the binary protocol and sqlx's NaiveDateTime
    -- decoder panics on the epoch addition (chrono overflow).
    last_created TIMESTAMP NOT NULL,
    last_id TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO play_history_walk_cursor (singleton, last_created, last_id)
VALUES (TRUE, '1970-01-01 00:00:00', '')
ON CONFLICT (singleton) DO NOTHING;

-- DEV-1984: games that permanently cannot be archived, with the reason.
-- Structural failures only (missing row, unparseable, null/zero frames);
-- transient errors retry via cja and never land here. Re-arming a game
-- after a fix takes BOTH deleting its row here AND making the walk see
-- the game again: rewind play_history_walk_cursor behind it (a rewind to
-- the seed re-walks ~270k done rows at 2,000/tick, ~11 h of cheap no-op
-- pages) or enqueue a BackupSingleGameJob for it directly. Once the
-- cursor has passed a game no tick revisits it, so deleting the row
-- alone does nothing — and erases the only record of why the game is
-- missing.
CREATE TABLE play_archive_exceptions (
    engine_game_id TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the walk cron so the first tick runs one interval after deploy,
-- not at boot (same rationale as 20261103210000 for TurnRetentionJob),
-- and so the pause lever is functional from deploy.
INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
VALUES (gen_random_uuid(), 'PlayHistoryWalkJob', NOW(), NOW(), NOW())
ON CONFLICT (name) DO NOTHING;
