SET LOCAL lock_timeout = '5s';

ALTER TABLE api_tokens ALTER COLUMN snake_access SET DEFAULT 'all';
