-- Shared login rate limits survive restarts and apply to all workers.
create table public_limits(key text primary key, expires_at integer not null, hits integer not null);
