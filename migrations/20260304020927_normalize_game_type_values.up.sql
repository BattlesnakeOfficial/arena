-- Normalize known game_type values from engine-native lowercase to canonical PascalCase.
-- Unknown game types (e.g., "solo", "wrapped") are intentionally left as-is.
UPDATE games SET game_type = 'Standard' WHERE lower(game_type) = 'standard' AND game_type != 'Standard';
UPDATE games SET game_type = 'Royale' WHERE lower(game_type) = 'royale' AND game_type != 'Royale';
UPDATE games SET game_type = 'Constrictor' WHERE lower(game_type) = 'constrictor' AND game_type != 'Constrictor';
UPDATE games SET game_type = 'Snail Mode' WHERE lower(game_type) IN ('snail_mode', 'snail mode') AND game_type != 'Snail Mode';

-- No board_size migration needed — readers now accept arbitrary "WxH" values via GameBoardSize::Custom.
