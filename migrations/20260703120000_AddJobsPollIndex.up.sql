-- Index supporting the job worker's "fetch next job" poll query.
--
-- Every poll cycle, `fetch_next_job` (crates/cja/src/jobs/worker.rs) runs:
--   SELECT job_id FROM jobs
--   WHERE run_at <= NOW()
--     AND (locked_by IS NULL OR locked_at < NOW() - <lock_timeout>)
--   ORDER BY priority DESC, run_at ASC, created_at ASC
--   LIMIT 1 FOR UPDATE SKIP LOCKED
--
-- With no index beyond the job_id primary key, this is a sequential scan plus a
-- sort on every poll. A composite btree matching the ORDER BY lets Postgres walk
-- rows in the desired order and return the first available job without sorting;
-- the leading `run_at` ordering within each priority group also helps skip
-- future-dated jobs. The lock-state disjunction is applied as a filter during the
-- ordered scan (it can't be indexed directly because of the OR), which is cheap
-- since the queue table stays small (completed jobs are deleted).
CREATE INDEX IF NOT EXISTS idx_jobs_fetch_next
  ON Jobs (priority DESC, run_at ASC, created_at ASC);
