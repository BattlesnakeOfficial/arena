-- Validate jobs_lock_pair_check in its own transaction so the scan does not
-- block writes (see 20261107120000_AddJobsLockPairCheck.sql).
ALTER TABLE jobs VALIDATE CONSTRAINT jobs_lock_pair_check;
