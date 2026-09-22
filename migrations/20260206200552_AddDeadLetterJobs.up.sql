CREATE TABLE IF NOT EXISTS dead_letter_jobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    original_job_id UUID NOT NULL,
    name TEXT NOT NULL,
    payload JSONB NOT NULL,
    context TEXT NOT NULL,
    priority INT NOT NULL,
    error_count INTEGER NOT NULL,
    last_error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
