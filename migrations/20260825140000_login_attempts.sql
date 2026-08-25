-- Login attempt history, for throttling.
--
-- Kept in the database rather than in process memory so a restart cannot reset
-- a lockout, which would otherwise be a free way to keep guessing.

CREATE TABLE login_attempts (
    id        UUID PRIMARY KEY,
    username  TEXT NOT NULL,
    client_ip TEXT,
    succeeded BOOLEAN NOT NULL,
    at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX login_attempts_username_idx ON login_attempts (username, at DESC);
CREATE INDEX login_attempts_ip_idx ON login_attempts (client_ip, at DESC);
CREATE INDEX login_attempts_at_idx ON login_attempts (at);
