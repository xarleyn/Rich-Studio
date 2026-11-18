-- Health keeps a failed first comment visible for a day from the failure itself.
alter table posts add column comment_failed_at integer;
update posts set comment_failed_at = coalesce(sent_at, created_at) + 86400
  where comment_status = 'failed';
