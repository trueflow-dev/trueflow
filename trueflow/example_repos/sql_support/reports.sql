-- reporting queries
WITH ranked_accounts AS (
  SELECT id, email
  FROM active_accounts
)
SELECT id, email
FROM ranked_accounts;

INSERT INTO audit_log (account_id, action)
SELECT id, 'emailed'
FROM active_accounts
WHERE email IS NOT NULL;

UPDATE accounts
SET deleted_at = '2024-01-01'
WHERE id IN (
  SELECT id
  FROM ranked_accounts
);

DELETE FROM audit_log
WHERE action = 'emailed';
