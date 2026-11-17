-- Preserve uncertain deliveries independently of the human-readable error text.
alter table posts add column outcome_unknown integer not null default 0;
update posts set outcome_unknown = 1
  where status = 'failed' and (instr(error, 'мог уйти') > 0 or instr(error, 'могло уйти') > 0);
