CREATE INDEX IF NOT EXISTS issues_updated_idx ON issues (updated_at DESC, id DESC); ANALYZE issues;
