-- Data change is not reversible (unrecognized values were cleared);
-- only the CHECK constraint is.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';
ALTER TABLE users DROP CONSTRAINT users_country_iso3166_alpha2;
