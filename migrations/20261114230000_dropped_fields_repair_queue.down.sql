DELETE FROM crons WHERE name = 'DroppedFieldsRepairSweepJob';
DROP TABLE IF EXISTS dropped_fields_repair_queue;
