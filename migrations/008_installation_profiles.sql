-- Migration 008: Installation Profiles
-- Create table to store installation profile configuration

CREATE TABLE IF NOT EXISTS installation_profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_type TEXT NOT NULL CHECK(profile_type IN ('individual', 'professional')),
    enabled_modules TEXT NOT NULL DEFAULT '[]', -- JSON array of enabled module names
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Insert default Individual profile
INSERT INTO installation_profiles (profile_type, enabled_modules, created_at, updated_at)
VALUES ('individual', '[]', datetime('now'), datetime('now'));

-- Add profile_id to library_config for reference
ALTER TABLE library_config ADD COLUMN profile_id INTEGER REFERENCES installation_profiles(id);

-- Set default profile for existing installations
UPDATE library_config SET profile_id = 1 WHERE profile_id IS NULL;
