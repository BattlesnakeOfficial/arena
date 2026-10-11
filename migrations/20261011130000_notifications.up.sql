CREATE TABLE notifications (
    notification_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    link TEXT,
    in_app_visible BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX notifications_drawer_idx
    ON notifications (user_id, created_at DESC, notification_id DESC)
    WHERE in_app_visible;

CREATE TABLE notification_emails (
    notification_id UUID PRIMARY KEY REFERENCES notifications(notification_id) ON DELETE CASCADE,
    subject TEXT,
    text TEXT,
    recipient_override TEXT,
    battlesnake_id UUID,
    claim_imported_account_id UUID,
    outcome TEXT CHECK (outcome IN ('sent', 'rate_limited', 'opted_out', 'transport_disabled', 'no_recipient')),
    finished_at TIMESTAMPTZ,
    CONSTRAINT notification_email_source CHECK (num_nonnulls(recipient_override, battlesnake_id) = 1),
    CONSTRAINT notification_email_content CHECK ((subject IS NULL) = (text IS NULL)),
    CONSTRAINT notification_email_verification CHECK ((text IS NULL) = (claim_imported_account_id IS NOT NULL)),
    CONSTRAINT notification_email_verification_recipient CHECK (claim_imported_account_id IS NULL OR recipient_override IS NOT NULL),
    CONSTRAINT notification_email_outcome CHECK ((outcome IS NULL) = (finished_at IS NULL))
);

CREATE TABLE notification_preferences (
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    channel TEXT NOT NULL CHECK (channel IN ('in_app', 'email')),
    enabled BOOLEAN NOT NULL,
    PRIMARY KEY (user_id, kind, channel)
);
