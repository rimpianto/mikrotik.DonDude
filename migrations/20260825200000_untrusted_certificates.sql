-- Allow a self-hosted Git remote (Gitea, GitLab, Forgejo) whose TLS certificate
-- the system does not trust.
--
-- Off by default and per-deployment, not per-remote, because there is only one
-- remote. Turning it on disables certificate verification for the backup push,
-- so it is a deliberate act and the UI says as much.

ALTER TABLE settings
    ADD COLUMN allow_invalid_certs BOOLEAN NOT NULL DEFAULT false;
