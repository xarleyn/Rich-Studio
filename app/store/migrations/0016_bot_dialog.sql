-- Private bot subscriptions (root DB) and notification outbox (current account DB).
create table bot_contacts (
    telegram_id integer primary key,
    language text not null default 'ru',
    mode text not null default 'off' check (mode in ('all', 'errors', 'off')),
    updated_at integer not null,
    revision integer not null default 0
);
create table bot_outbox (
    event_key text primary key,
    recipient integer not null,
    category text not null,
    body text not null,
    state text not null default 'pending',
    attempts integer not null default 0,
    retry_at integer not null default 0,
    created_at integer not null
);
create index bot_outbox_due on bot_outbox(state, retry_at);
