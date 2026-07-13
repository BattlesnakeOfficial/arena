-- Curated language/platform tags for battlesnakes (BS-653af513).
-- The tag catalog is a moderated, DB-defined set (no free text): users pick
-- from it, and new tags are added by request. `battlesnake_tags` joins
-- snakes to tags; a snake may carry multiple tags from the same category
-- (e.g. two languages), capped at 5 total server-side.
CREATE TABLE tags (
    tag_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    name TEXT UNIQUE NOT NULL CHECK (name <> ''),
    category TEXT NOT NULL CHECK (category IN ('language', 'platform')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TRIGGER update_tags_updated_at BEFORE
UPDATE ON tags FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE battlesnake_tags (
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes (battlesnake_id) ON DELETE CASCADE,
    tag_id UUID NOT NULL REFERENCES tags (tag_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (battlesnake_id, tag_id)
);

CREATE INDEX battlesnake_tags_battlesnake_id_idx ON battlesnake_tags (battlesnake_id);

CREATE INDEX battlesnake_tags_tag_id_idx ON battlesnake_tags (tag_id);

-- Seed the initial curated set
INSERT INTO tags (name, category)
VALUES
    ('Python', 'language'),
    ('Rust', 'language'),
    ('Go', 'language'),
    ('TypeScript', 'language'),
    ('JavaScript', 'language'),
    ('Java', 'language'),
    ('C#', 'language'),
    ('Elixir', 'language'),
    ('Ruby', 'language'),
    ('Kotlin', 'language'),
    ('AWS', 'platform'),
    ('GCP', 'platform'),
    ('Azure', 'platform'),
    ('Fly.io', 'platform'),
    ('Railway', 'platform'),
    ('Render', 'platform'),
    ('Cloudflare', 'platform'),
    ('Self-hosted', 'platform'),
    ('Raspberry Pi', 'platform'),
    ('Heroku', 'platform');
