-- Add parent_id to tags table for hierarchy
-- It is nullable (root tags have NULL parent_id)
-- It references the same table (self-referential)

ALTER TABLE tags ADD COLUMN parent_id INTEGER REFERENCES tags(id) ON DELETE SET NULL;
