-- games.status domain, part 2 of 2 (DEV-2177): separate transaction from
-- the NOT VALID add so the validation scan does not run under the ALTER's
-- ACCESS EXCLUSIVE lock. VALIDATE takes SHARE UPDATE EXCLUSIVE: the
-- serving revision's reads and writes continue. Fails (and rolls back the
-- deploy) if any existing row violates the CHECK — prod was verified
-- violation-free before deploy; see the PR for the read-only check and
-- scan timing.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '120s';
ALTER TABLE games VALIDATE CONSTRAINT games_status_domain;
