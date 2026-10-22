SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';
ALTER TABLE battlesnakes DROP COLUMN hosted_by_oauth_client_id;
DROP TABLE oauth_client_hosting_domains;
ALTER TABLE oauth_clients DROP COLUMN description;
ALTER TABLE oauth_clients DROP COLUMN onboarding_url;
