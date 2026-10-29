-- The root database owns Telegram identities. Account databases use the same versioned schema.
create table accounts(
  telegram_id integer primary key, first_name text not null, last_name text, username text,
  created_at integer not null, updated_at integer not null);
create table account_sessions(
  hash text primary key, telegram_id integer not null, expires_at integer not null);
create index account_sessions_expiry on account_sessions(expires_at);
create table login_states(hash text primary key, expires_at integer not null);
