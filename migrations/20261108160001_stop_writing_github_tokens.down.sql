-- The blanking UPDATE is irreversible; only the default is undone.
ALTER TABLE users ALTER COLUMN github_access_token DROP DEFAULT;
