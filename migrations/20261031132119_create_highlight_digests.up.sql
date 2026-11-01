-- Weekly arena highlights digests (DEV-1993): one stored digest per aligned
-- week (7 days ending Saturday 12:00:00 UTC), its sections and items, and
-- the one-shot Discord announcement state.

CREATE TABLE highlight_digests (
    digest_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    window_start TIMESTAMPTZ NOT NULL,
    window_end TIMESTAMPTZ NOT NULL,
    games_count INT NOT NULL,
    duel_calibration_k DOUBLE PRECISION NOT NULL,
    four_snake_calibration_k DOUBLE PRECISION NOT NULL,
    discord_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (discord_status IN ('pending', 'posted', 'skipped', 'abandoned')),
    discord_message TEXT NOT NULL,
    discord_posted_at TIMESTAMPTZ,
    computed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (window_end),
    -- Subtraction form: `window_end = window_start + interval` adds calendar
    -- days in the session time zone and drifts across DST; subtraction is
    -- correct in any session.
    CHECK (window_end - window_start = INTERVAL '7 days'),
    CHECK (
        EXTRACT(ISODOW FROM window_end AT TIME ZONE 'UTC') = 6
        AND EXTRACT(HOUR FROM window_end AT TIME ZONE 'UTC') = 12
        AND EXTRACT(MINUTE FROM window_end AT TIME ZONE 'UTC') = 0
        AND EXTRACT(SECOND FROM window_end AT TIME ZONE 'UTC') = 0
    )
);

CREATE TRIGGER update_highlight_digests_updated_at
    BEFORE UPDATE ON highlight_digests
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE INDEX highlight_digests_window_start_idx ON highlight_digests (window_start DESC);
CREATE INDEX highlight_digests_pending_idx ON highlight_digests (window_end)
    WHERE discord_status = 'pending';

CREATE TABLE highlight_sections (
    section_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    digest_id UUID NOT NULL REFERENCES highlight_digests (digest_id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('throne', 'movers', 'newcomers', 'upsets',
        'photo_finishes', 'mutual_destruction', 'chicken', 'fumes', 'hat_tricks',
        'marathons', 'streaks', 'rivalries', 'ops_footnote', 'calibration')),
    ladder_id UUID NULL REFERENCES leaderboards (leaderboard_id),
    position INT NOT NULL,
    summary TEXT NOT NULL,
    UNIQUE NULLS NOT DISTINCT (digest_id, kind, ladder_id)
);

CREATE TABLE highlight_items (
    item_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    section_id UUID NOT NULL REFERENCES highlight_sections (section_id) ON DELETE CASCADE,
    position INT NOT NULL,
    game_id UUID NULL REFERENCES games (game_id) ON DELETE SET NULL,
    subject TEXT NULL,
    value DOUBLE PRECISION NULL,
    text TEXT NOT NULL,
    UNIQUE (section_id, position)
);
