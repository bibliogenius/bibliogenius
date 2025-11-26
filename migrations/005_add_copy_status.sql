-- Migration 005: Add status and is_temporary to copies
-- Adds book status tracking and temporary book flag

-- Add status column to copies
ALTER TABLE copies ADD COLUMN status TEXT NOT NULL DEFAULT 'available';

-- Add is_temporary column to copies
ALTER TABLE copies ADD COLUMN is_temporary INTEGER NOT NULL DEFAULT 0;

-- Create index for quick status lookup
CREATE INDEX IF NOT EXISTS idx_copies_status ON copies(status);
CREATE INDEX IF NOT EXISTS idx_copies_temporary ON copies(is_temporary);
