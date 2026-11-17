-- Per-button failures of ephemeral rich replies; failed replies are never retried.
alter table posts add column reply_errors text;
