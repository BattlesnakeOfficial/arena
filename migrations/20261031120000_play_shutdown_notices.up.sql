-- play.battlesnake.com shutdown notice emails (DEV-1983).

-- Set by the one-click unsubscribe link in a notice; later waves skip the
-- account. Notices go to imported play accounts, which may never have an
-- arena user, so this can't live in notification_preferences.
ALTER TABLE imported_accounts ADD COLUMN shutdown_notices_unsubscribed_at TIMESTAMPTZ;

-- One row per (account, wave), written as 'sending' BEFORE the email goes
-- out. The primary key makes each wave at-most-once per account: a crash
-- mid-send leaves 'sending' and the account is never emailed that wave again.
CREATE TABLE play_shutdown_notices (
    imported_account_id UUID NOT NULL REFERENCES imported_accounts (imported_account_id) ON DELETE CASCADE,
    wave SMALLINT NOT NULL CHECK (wave BETWEEN 1 AND 3),
    status TEXT NOT NULL CHECK (status IN ('sending', 'sent', 'failed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (imported_account_id, wave)
);

CREATE INDEX play_shutdown_notices_wave_status_idx ON play_shutdown_notices (wave, status);
