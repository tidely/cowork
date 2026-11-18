-- The host's first model-content timestamp, used only for displayed work
-- duration. Request timing remains unchanged for usage statistics.
ALTER TABLE agent_runs ADD COLUMN first_content_at TEXT
    CHECK (first_content_at IS NULL OR
           (first_content_at LIKE '%+00:00' AND unixepoch(first_content_at) IS NOT NULL));

-- Older runs have no recorded startup latency; retain their displayed duration.
UPDATE agent_runs SET first_content_at = started_at;
