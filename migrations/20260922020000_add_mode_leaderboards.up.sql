ALTER TABLE leaderboards
    ADD COLUMN game_type TEXT NOT NULL DEFAULT 'Standard'
        CHECK (game_type IN ('Standard', 'Royale', 'Constrictor', 'Snail Mode')),
    ADD COLUMN board_size TEXT NOT NULL DEFAULT '11x11'
        CHECK (board_size IN ('7x7', '11x11', '19x19')),
    ADD COLUMN match_size INT NOT NULL DEFAULT 4
        CHECK (match_size BETWEEN 2 AND 4);

INSERT INTO leaderboards (leaderboard_id, name, game_type, board_size, match_size)
SELECT '77ce41bf-12ad-4eac-bac2-b11939fb9b18', 'Royale 11x11', 'Royale', '11x11', 4
WHERE NOT EXISTS (SELECT 1 FROM leaderboards WHERE name = 'Royale 11x11');

INSERT INTO leaderboards (leaderboard_id, name, game_type, board_size, match_size)
SELECT '77212f22-20f5-4e0c-8db6-78844318a9bc', 'Duels 11x11', 'Standard', '11x11', 2
WHERE NOT EXISTS (SELECT 1 FROM leaderboards WHERE name = 'Duels 11x11');
