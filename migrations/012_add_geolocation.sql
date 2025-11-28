ALTER TABLE library_config ADD COLUMN latitude REAL;
ALTER TABLE library_config ADD COLUMN longitude REAL;
ALTER TABLE library_config ADD COLUMN share_location BOOLEAN DEFAULT FALSE;
