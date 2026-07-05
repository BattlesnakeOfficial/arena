-- Email safety net + email-recovery claim path (BS-7e38).

-- Every attempted transactional send, recorded BEFORE the per-recipient
-- rate-limit check (record-before-check, same race-safe pattern as
-- claim_attempts): concurrent sends see each other, and a suppressed send
-- still spends budget so hammering a trigger never earns extra email.
CREATE TABLE email_log (
    email_log_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    recipient TEXT NOT NULL,
    -- What kind of message, e.g. 'account_claimed', 'claim_verification',
    -- 'matchmaking_deactivated'. Informational; the limit is per recipient
    -- across all purposes.
    purpose TEXT NOT NULL,
    sent_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX email_log_recipient_idx ON email_log (lower(recipient), sent_at);

-- One-time magic-link tokens for the email-recovery claim path: the
-- last-resort flow for play users with no usable password and no GitHub
-- link. Only the SHA-256 of the secret is stored (leaked DB rows can't be
-- replayed as links). `used_at` is the single-use compare-and-set.
CREATE TABLE claim_email_tokens (
    claim_email_token_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    token_hash TEXT NOT NULL UNIQUE,
    imported_account_id UUID NOT NULL REFERENCES imported_accounts (imported_account_id) ON DELETE CASCADE,
    -- The logged-in arena user who requested the link. Completing the claim
    -- requires the SAME user, so a forwarded/intercepted link can never
    -- attach the play account to someone else's arena login.
    requested_by_user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
