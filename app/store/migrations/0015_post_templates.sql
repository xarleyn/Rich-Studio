-- Reusable post templates and their configurable fields, isolated by the account database.
create table post_templates(
  id text primary key,
  title text not null,
  payload text not null,
  fields text not null,
  version integer not null default 1,
  created_at integer not null,
  updated_at integer not null
);
create index post_templates_updated on post_templates(updated_at);
