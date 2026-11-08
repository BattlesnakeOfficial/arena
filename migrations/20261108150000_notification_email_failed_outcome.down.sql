ALTER TABLE notification_emails
    DROP CONSTRAINT notification_email_outcome_values,
    ADD CONSTRAINT notification_emails_outcome_check
    CHECK (outcome IN ('sent', 'rate_limited', 'opted_out', 'transport_disabled', 'no_recipient'));
