-- Сессия владельца, открытая Telegram-входом по TG_OWNER_ID: живёт в корневой таблице sessions
-- с заполненным telegram_id вместо token_name/key_hash и перестаёт действовать, когда настройка
-- меняется или снимается (как ключевые сессии умирают при смене STUDIO_API_KEY).
alter table sessions add column telegram_id integer;
