-- Per-snake opt-out of tournament leaderboard imports. Only consulted for
-- public snakes owned by someone other than the tournament organizer;
-- private snakes are never importable by others regardless.
ALTER TABLE battlesnakes
    ADD COLUMN allow_tournament_import BOOLEAN NOT NULL DEFAULT TRUE;
