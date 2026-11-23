ALTER TABLE settings ADD COLUMN web_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE settings ADD COLUMN firecrawl_endpoint TEXT;
