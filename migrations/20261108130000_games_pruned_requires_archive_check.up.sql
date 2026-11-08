-- DEV-2178: a pruned game (turns_pruned_at set) must keep the archive
-- pointers that name its only surviving frame copy. NOT VALID so the
-- ADD is catalog-only (brief ACCESS EXCLUSIVE); the NEXT migration
-- validates under SHARE UPDATE EXCLUSIVE without blocking DML —
-- same split as 20260929124854/55.
SET LOCAL lock_timeout = '5s';

ALTER TABLE games
    ADD CONSTRAINT games_pruned_requires_archive_check
    CHECK (turns_pruned_at IS NULL OR turns_pruned_at IS NOT NULL)
    NOT VALID;
