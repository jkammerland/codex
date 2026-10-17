-- Board projections share the record transaction with immutable messages and
-- operation receipts. Index their independent append order and conversation.
CREATE INDEX board_entry_sequence ON record(json_extract(payload, '$.appendSeq'))
    WHERE namespace = 'board_entry';
CREATE INDEX board_entry_conversation_sequence ON record(
    json_extract(payload, '$.conversationId'), json_extract(payload, '$.appendSeq'))
    WHERE namespace = 'board_entry';
CREATE INDEX board_follower_conversation ON record(json_extract(payload, '$.conversationId'))
    WHERE namespace = 'board_follower';
CREATE INDEX board_read_conversation ON record(json_extract(payload, '$.conversationId'))
    WHERE namespace = 'board_read';
