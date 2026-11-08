-- games.status domain, part 1 of 2 (DEV-2177): CHECK added NOT VALID so
-- the ACCESS EXCLUSIVE lock covers only a metadata-only change (no scan
-- of the ~6M-row table). A NOT VALID CHECK still enforces on every new
-- write; part 2 VALIDATEs existing rows under SHARE UPDATE EXCLUSIVE.
-- Mirrors GameStatus in server/src/models/game.rs and the users.country
-- CHECK precedent (20261023130000).
SET LOCAL lock_timeout = '5s';
ALTER TABLE games
    ADD CONSTRAINT games_status_domain
    CHECK (status IN ('waiting', 'running', 'finished', 'failed')) NOT VALID;
