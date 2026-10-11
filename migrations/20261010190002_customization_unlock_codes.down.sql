DROP TABLE customization_code_failed_attempts;
DROP TABLE customization_code_redemptions;
DROP TABLE customization_unlock_codes;
DELETE FROM customization_grants WHERE source = 'code';
ALTER TABLE customization_grants DROP CONSTRAINT customization_grants_source_check;
ALTER TABLE customization_grants ADD CONSTRAINT customization_grants_source_check
    CHECK (source IN ('pre_token', 'play_import', 'admin', 'token', 'achievement'));
