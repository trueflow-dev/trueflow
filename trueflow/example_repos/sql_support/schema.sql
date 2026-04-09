-- bootstrap base objects
CREATE TABLE accounts (
  id bigint PRIMARY KEY,
  email text NOT NULL,
  status account_status NOT NULL,
  updated_at timestamp
);

CREATE TYPE account_status AS ENUM ('active', 'disabled');

CREATE VIEW active_accounts AS
SELECT id, email, status
FROM accounts
WHERE status = 'active';

CREATE FUNCTION normalize_email(input text)
RETURNS text
LANGUAGE sql
AS $$
  SELECT lower(trim(input));
$$;

CREATE TRIGGER accounts_set_updated_at
BEFORE UPDATE ON accounts
FOR EACH ROW
EXECUTE FUNCTION touch_updated_at();

ALTER TABLE accounts
  ADD COLUMN deleted_at timestamp;

DROP VIEW IF EXISTS legacy_accounts;

SELECT id, email
FROM active_accounts
WHERE email IS NOT NULL;

INSERT INTO accounts (id, email, status)
VALUES (1, 'demo@example.com', 'active');

UPDATE accounts
SET status = 'disabled'
WHERE id = 2;

DELETE FROM accounts
WHERE status = 'disabled';
