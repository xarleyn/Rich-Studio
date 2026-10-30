-- Personal API credentials are looked up before entering an account scope: root database only.
create table account_tokens(
  id text primary key, telegram_id integer not null, name text not null,
  hash text not null unique, level text not null check(level in ('read', 'draft', 'publish')),
  created_at integer not null, expires_at integer, last_used_at integer, revoked_at integer);
create index account_tokens_owner on account_tokens(telegram_id, created_at);
