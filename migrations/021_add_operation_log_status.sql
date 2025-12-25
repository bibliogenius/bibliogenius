ALTER TABLE operation_log ADD COLUMN status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE operation_log ADD COLUMN error_message TEXT;

