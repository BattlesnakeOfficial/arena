-- A job row must carry both lock columns or neither. `locked_by` set with
-- `locked_at` NULL is never claimable (the claim's `locked_at < ...`
-- predicate is NULL) and cja never writes that combination. Added NOT VALID
-- so this file holds ACCESS EXCLUSIVE only for the catalog change; the next
-- migration validates in its own transaction under SHARE UPDATE EXCLUSIVE.
ALTER TABLE jobs
  ADD CONSTRAINT jobs_lock_pair_check
  CHECK ((locked_by IS NULL) = (locked_at IS NULL))
  NOT VALID;
