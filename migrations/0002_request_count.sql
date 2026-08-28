-- Adds a per-artifact request counter.
--
-- The counter starts at zero for every cached artifact. Each client request
-- increments it for the requested path via `bump_request_count` in
-- `sql/cache.sql`. It is recorded only and is not consumed by eviction,
-- statistics, or any other behavior yet.
--
-- `upsert_artifact` intentionally leaves the counter untouched, so refreshing
-- or reinstalling content for a path keeps its request history.

ALTER TABLE artifacts ADD COLUMN request_count INTEGER NOT NULL DEFAULT 0;