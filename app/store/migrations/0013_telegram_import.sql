-- One explicitly armed forward import per account; no Telegram session secrets in the database/export.
create table telegram_import_inbox(
  id text primary key, source_url text not null, sender_id integer not null,
  expires_at integer not null, state text not null, result text, error text);
