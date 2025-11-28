-- Migration 009: Professional Cataloguing Fields
-- Add optional professional metadata to books table

ALTER TABLE books ADD COLUMN dewey_decimal TEXT;
ALTER TABLE books ADD COLUMN lcc TEXT; -- Library of Congress Classification
ALTER TABLE books ADD COLUMN subjects TEXT DEFAULT '[]'; -- JSON array of subject headings
ALTER TABLE books ADD COLUMN marc_record TEXT; -- Full MARC record (XML or JSON)
ALTER TABLE books ADD COLUMN cataloguing_notes TEXT; -- Internal notes for librarians
