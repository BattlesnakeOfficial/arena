-- Per-user customization ownership. The catalog itself (which heads/tails
-- exist, their groups and costs) is code-defined in
-- server/src/customizations/catalog.rs; grants reference it by
-- (customization_type, slug). Mirrors play's core_snakecustomizationgrant
-- so play grants can be imported 1:1 during account migration.
CREATE TABLE customization_grants (
    customization_grant_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    customization_type TEXT NOT NULL CHECK (customization_type IN ('head', 'tail')),
    slug TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, customization_type, slug)
);
