-- Session loads and MFA checks fetch only an account's enabled factors.
CREATE INDEX user_mfa_enabled_user ON user_mfa(user_id) WHERE enabled_at IS NOT NULL;
