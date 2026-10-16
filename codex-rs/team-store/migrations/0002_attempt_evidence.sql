-- Earlier hosts associated completion with a reusable turn ID. Preserve the
-- delivery and history, while holding outcomes that lack request evidence.
UPDATE delivery
SET receipt = json_set(
    receipt,
    '$.state', 'needsReconciliation',
    '$.detail', 'Legacy delivery has no model-request participation evidence; inspect the original attempt.'
)
WHERE json_extract(receipt, '$.state') IN ('accepted', 'completed', 'interrupted', 'needsReconciliation')
  AND NOT EXISTS (
      SELECT 1 FROM record
      WHERE namespace = 'delivery_attempt' AND record.id = delivery.id
  );

INSERT INTO outbox(id, kind, payload)
SELECT 'receipt:migration-v2:' || id, 'receipt', receipt
FROM delivery
WHERE json_extract(receipt, '$.detail') = 'Legacy delivery has no model-request participation evidence; inspect the original attempt.';
