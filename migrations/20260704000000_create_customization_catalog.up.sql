-- Customization catalog: which head/tail cosmetics exist, grouped by
-- availability. Mirrors play's core_snakecustomization{,group} so grants can
-- be imported 1:1 during account migration. Colors stay free-form hex on the
-- battlesnake row and are not catalog entries.
CREATE TABLE customization_groups (
    customization_group_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    -- everyone: free for all, no grant needed
    -- restricted: needs a grant unless cost = 0
    -- hidden: not shown in UI, grant-only
    -- preview: shown in UI, grant-only (not purchasable)
    availability TEXT NOT NULL DEFAULT 'everyone'
        CHECK (availability IN ('everyone', 'restricted', 'hidden', 'preview')),
    ordinal INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TRIGGER update_customization_groups_updated_at BEFORE
UPDATE ON customization_groups FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE customizations (
    customization_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    customization_type TEXT NOT NULL CHECK (customization_type IN ('head', 'tail')),
    slug TEXT NOT NULL,
    display_name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    image_url TEXT NOT NULL DEFAULT '',
    customization_group_id UUID NOT NULL
        REFERENCES customization_groups (customization_group_id) ON DELETE RESTRICT,
    -- Points price carried over from play for forward-compat; the points
    -- economy itself is not ported yet, so cost only distinguishes
    -- restricted-free (0) from grant-required (> 0).
    cost INTEGER NOT NULL DEFAULT 0 CHECK (cost >= 0),
    release_date TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (customization_type, slug)
);

CREATE INDEX customizations_group_id_idx ON customizations (customization_group_id);

CREATE TRIGGER update_customizations_updated_at BEFORE
UPDATE ON customizations FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE customization_grants (
    customization_grant_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    customization_id UUID NOT NULL
        REFERENCES customizations (customization_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, customization_id)
);

-- user_id lookups are covered by the leading column of
-- UNIQUE (user_id, customization_id).
CREATE INDEX customization_grants_customization_id_idx
    ON customization_grants (customization_id);
