-- Prune bookkeeping (DEV-1981): when a native game's turns were deleted
-- after archive-content confirmation. Nullable ADD COLUMN; rows leave
-- games_turns_prunable_idx once set.
ALTER TABLE games ADD COLUMN turns_pruned_at TIMESTAMPTZ;
