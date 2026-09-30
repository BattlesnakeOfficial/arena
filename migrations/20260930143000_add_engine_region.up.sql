ALTER TABLE battlesnakes
  ADD COLUMN engine_region TEXT NOT NULL DEFAULT 'us-west1'
    CHECK (engine_region IN ('us-west1', 'us-east4', 'europe-west4'));

ALTER TABLE imported_snakes
  ADD COLUMN engine_region TEXT NOT NULL DEFAULT 'us-west1'
    CHECK (engine_region IN ('us-west1', 'us-east4', 'europe-west4'));
