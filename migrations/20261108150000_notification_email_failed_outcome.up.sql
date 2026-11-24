-- DEV-2179: terminal outcome for permanent Mailgun rejections. The
-- unnamed column CHECK gets a name so future changes can target it.
ALTER TABLE notification_emails
    DROP CONSTRAINT notification_emails_outcome_check,
    ADD CONSTRAINT notification_email_outcome_values
    CHECK (outcome IN ('sent', 'rate_limited', 'opted_out', 'transport_disabled', 'no_recipient', 'failed'));
