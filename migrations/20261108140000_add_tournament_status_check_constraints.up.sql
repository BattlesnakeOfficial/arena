-- Pin the free-TEXT lifecycle columns from 20260702000000_create_tournament_tables
-- to their Rust enum value sets. Prod values verified read-only before adding
-- (2026-11-07): all rows already satisfy these constraints.
-- NOTE: adding a new enum value to TournamentStatus/RegistrationStatus/
-- MatchStatus now requires a constraint-widening migration to deploy BEFORE
-- code that writes the new value ships (rolling-deploy order).

ALTER TABLE tournaments
    ADD CONSTRAINT tournaments_status_check
    CHECK (status IN ('created', 'registration', 'in_progress', 'completed', 'canceled'));

ALTER TABLE tournaments
    ADD CONSTRAINT tournaments_registration_status_check
    CHECK (registration_status IN ('open', 'closed', 'owner_only'));

ALTER TABLE tournament_matches
    ADD CONSTRAINT tournament_matches_status_check
    CHECK (status IN ('scheduled', 'in_progress', 'completed', 'canceled'));
