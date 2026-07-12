-- Two-axis appearance preference (site redesign, Direction E):
-- site_theme controls every page; theater_theme controls game replay/live
-- pages, which can diverge from the site theme (e.g. light site, dark theater).
ALTER TABLE users
  ADD COLUMN site_theme TEXT NOT NULL DEFAULT 'system'
    CHECK (site_theme IN ('system', 'light', 'dark')),
  ADD COLUMN theater_theme TEXT NOT NULL DEFAULT 'dark'
    CHECK (theater_theme IN ('match', 'dark', 'light'));
