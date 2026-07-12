-- Normalize stored snake colors to strict lowercase #rrggbb, matching
-- customizations::normalize_color. Play-import copied legacy colors verbatim,
-- so rows can hold arbitrary strings. Invalid values become '' — the board
-- generates a stable per-snake fallback for empty, and the UI chip renderer
-- falls back to a neutral gray.
UPDATE battlesnakes SET color = lower(color)
WHERE color ~ '^#[0-9a-fA-F]{6}$' AND color <> lower(color);
UPDATE battlesnakes SET color = ''
WHERE color !~ '^#[0-9a-fA-F]{6}$' AND color <> '';

-- Staging table too, so future materializations start clean.
UPDATE imported_snakes SET color = lower(color)
WHERE color ~ '^#[0-9a-fA-F]{6}$' AND color <> lower(color);
UPDATE imported_snakes SET color = ''
WHERE color !~ '^#[0-9a-fA-F]{6}$' AND color <> '';
