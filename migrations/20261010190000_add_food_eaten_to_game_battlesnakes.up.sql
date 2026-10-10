-- Food eaten per snake, counted by the game runner from the engine's feed
-- stage and written with the placement when the game finishes. NULL until
-- then, and for games finished before this column existed.
ALTER TABLE game_battlesnakes ADD COLUMN food_eaten INT NULL;
