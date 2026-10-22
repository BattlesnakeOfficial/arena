-- Hosting partners: extra oauth_clients fields, registered hosting
-- domains, and the per-snake hosted-by mark. Nothing here is visible
-- until an admin creates a hosting_provider client.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

ALTER TABLE oauth_clients ADD COLUMN onboarding_url TEXT;
ALTER TABLE oauth_clients ADD COLUMN description TEXT;

-- Registered hosting domains: a snake URL matches a client when its host
-- equals a row here or is a subdomain of one. The CHECK keeps domains
-- canonical (lowercase, ASCII, DNS-label shape, >=2 labels) so the
-- matching in models::battlesnake::update_battlesnake is plain string
-- comparison. ASCII-only also means IDN homographs can never register.
CREATE TABLE oauth_client_hosting_domains (
    oauth_client_id UUID NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
    domain TEXT NOT NULL CHECK (
        domain = lower(domain)
        AND domain ~ '^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$'
    ),
    PRIMARY KEY (oauth_client_id, domain)
);

-- The "Hosted on X" mark. Cleared automatically whenever the snake's
-- URL leaves the hosting client's registered domains (enforced inside
-- models::battlesnake::update_battlesnake, the single URL update path).
ALTER TABLE battlesnakes
    ADD COLUMN hosted_by_oauth_client_id UUID REFERENCES oauth_clients(id);
-- Reverse lookups (admin/partner "which snakes does this client host")
-- have no query yet; the index is cheap and mostly-NULL.
CREATE INDEX battlesnakes_hosted_by_oauth_client_id_idx
    ON battlesnakes (hosted_by_oauth_client_id);
