DELETE FROM leaderboards WHERE leaderboard_id IN (
    '77ce41bf-12ad-4eac-bac2-b11939fb9b18',
    '77212f22-20f5-4e0c-8db6-78844318a9bc'
);

ALTER TABLE leaderboards
    DROP COLUMN match_size,
    DROP COLUMN board_size,
    DROP COLUMN game_type;
