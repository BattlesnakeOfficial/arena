-- Record move responses that arrived but were unusable (non-2xx status, a
-- body that isn't a move response, or an unrecognized direction). A snake
-- that crashes with 500s used to look healthy here: only `timed_out`
-- (no answer at all) was stored. The health sweeper reads both flags to
-- decide which leaderboard entries need probing. Engine-proxy faults set
-- neither.
--
-- A constant default is metadata-only (no table rewrite); the lock timeout
-- keeps the brief ACCESS EXCLUSIVE lock from queueing live game writes
-- behind it indefinitely.
SET LOCAL lock_timeout = '5s';

ALTER TABLE snake_turns
    ADD COLUMN errored BOOLEAN NOT NULL DEFAULT FALSE;
