SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

-- One rendered clip per achievement earn: the earn's highlight GIF,
-- produced by the background sweep/render jobs and served by the share
-- page. Keyed by earn (not game) so clip URLs and objects carry no game
-- id; CASCADE on the earn keeps reachability honest when a user is
-- deleted (the bucket keeps the orphaned bytes; the route 404s).
CREATE TABLE achievement_clips (
    achievement_earn_id UUID PRIMARY KEY
        REFERENCES achievement_earns(achievement_earn_id) ON DELETE CASCADE,
    -- pending: awaiting or retrying a render; rendered: object exists and
    -- serves; failed: attempts exhausted or nothing to render, terminal.
    status TEXT NOT NULL CHECK (status IN ('pending', 'rendered', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    byte_size INTEGER,
    render_ms INTEGER,
    last_attempt_at TIMESTAMPTZ,
    rendered_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- A rendered row always carries its result.
    CHECK (status <> 'rendered'
           OR (byte_size IS NOT NULL AND render_ms IS NOT NULL AND rendered_at IS NOT NULL))
);

-- The sweep's stale re-drive and terminal-fail steps both scan pending
-- rows by age (attempt filters evaluated in the query).
CREATE INDEX achievement_clips_pending_idx
    ON achievement_clips (updated_at)
    WHERE status = 'pending';

CREATE TRIGGER update_achievement_clips_updated_at
    BEFORE UPDATE ON achievement_clips
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
