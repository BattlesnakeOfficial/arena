-- Constrictor ladder, matching play.battlesnake.com's: 11x11, four snakes.
INSERT INTO leaderboards (leaderboard_id, name, game_type, board_size, match_size)
SELECT '2997e28a-83a0-41e0-a203-d42108cfe450', 'Constrictor 11x11', 'Constrictor', '11x11', 4
WHERE NOT EXISTS (SELECT 1 FROM leaderboards WHERE name = 'Constrictor 11x11');
