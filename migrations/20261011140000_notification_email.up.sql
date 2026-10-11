CREATE TABLE notification_email_requests (
    request_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    token_hash TEXT UNIQUE,
    expires_at TIMESTAMPTZ,
    used_at TIMESTAMPTZ,
    sent_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT notification_email_request_token CHECK ((token_hash IS NULL) = (expires_at IS NULL))
);
CREATE INDEX notification_email_requests_created_idx ON notification_email_requests (user_id, created_at DESC);
CREATE INDEX notification_email_requests_sent_idx ON notification_email_requests (user_id, sent_at DESC) WHERE sent_at IS NOT NULL;

ALTER TABLE users ADD COLUMN notification_email TEXT;
ALTER TABLE users ADD COLUMN pending_notification_email_request_id UUID REFERENCES notification_email_requests(request_id) ON DELETE SET NULL;

ALTER TABLE notification_emails ADD COLUMN notification_email_request_id UUID REFERENCES notification_email_requests(request_id) ON DELETE CASCADE;
ALTER TABLE notification_emails ADD COLUMN recipient_user BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE notification_emails DROP CONSTRAINT notification_email_source;
ALTER TABLE notification_emails ADD CONSTRAINT notification_email_source CHECK (num_nonnulls(recipient_override, battlesnake_id) + recipient_user::int = 1);
ALTER TABLE notification_emails DROP CONSTRAINT notification_email_verification;
ALTER TABLE notification_emails ADD CONSTRAINT notification_email_verification CHECK ((text IS NULL) = (num_nonnulls(claim_imported_account_id, notification_email_request_id) = 1));
ALTER TABLE notification_emails ADD CONSTRAINT notification_email_request_exclusive CHECK (num_nonnulls(claim_imported_account_id, notification_email_request_id) <= 1);
ALTER TABLE notification_emails ADD CONSTRAINT notification_email_request_recipient CHECK (notification_email_request_id IS NULL OR recipient_override IS NOT NULL);
