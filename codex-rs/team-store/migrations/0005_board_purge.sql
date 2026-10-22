-- Numbered migration fences readers that could replay content removed by a
-- protocol-5 coordinator. Content-free journals live in the existing record table.
CREATE INDEX board_purge_revision ON record(json_extract(payload, '$.revision'))
WHERE namespace = 'board_purge_journal';
