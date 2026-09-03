-- Email notifications for scheduled runs.

-- SMTP relay settings. The password is sealed with DONDUDE_MASTER_KEY like
-- every other credential; the UI never renders it back.
ALTER TABLE settings ADD COLUMN
    smtp_host  TEXT;
ALTER TABLE settings ADD COLUMN
    smtp_port  INTEGER NOT NULL DEFAULT 465
        CHECK (smtp_port BETWEEN 1 AND 65535);
ALTER TABLE settings ADD COLUMN
    smtp_username TEXT;
ALTER TABLE settings ADD COLUMN
    smtp_password_sealed TEXT;
ALTER TABLE settings ADD COLUMN
    notify_from TEXT;
ALTER TABLE settings ADD COLUMN
    notify_to   TEXT;

-- true: only failed runs send mail (quiet when everything is fine).
-- false (default): every scheduled run reports — a daily heartbeat.
ALTER TABLE settings ADD COLUMN
    notify_on_failure_only BOOLEAN NOT NULL DEFAULT false;
