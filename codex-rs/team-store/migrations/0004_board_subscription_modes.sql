-- This numbered migration also fences older SQLx binaries from reopening a
-- store whose passive subscriptions their routing code cannot understand.
UPDATE record
SET payload = json_set(payload, '$.mode',
    CASE json_extract(payload, '$.identity.type') WHEN 'user' THEN 'watch' ELSE 'wake' END)
WHERE namespace IN ('board_follower', 'board_topic')
  AND json_type(payload) = 'object'
  AND json_type(payload, '$.mode') IS NULL;

-- Worker caches must retain their last committed snapshot while disconnected.
-- Upgrade metadata only; never rewrite outboxes or their request digests.
UPDATE record
SET payload = json_set(payload,
    '$.followers', json((SELECT json_group_array(json(
        CASE WHEN json_type(value, '$.mode') IS NULL THEN
            json_set(value, '$.mode', CASE json_extract(value, '$.identity.type')
                WHEN 'user' THEN 'watch' ELSE 'wake' END)
        ELSE value END)) FROM json_each(payload, '$.followers'))),
    '$.topicSubscriptions', json((SELECT json_group_array(json(
        CASE WHEN json_type(value, '$.mode') IS NULL THEN
            json_set(value, '$.mode', CASE json_extract(value, '$.identity.type')
                WHEN 'user' THEN 'watch' ELSE 'wake' END)
        ELSE value END)) FROM json_each(payload, '$.topicSubscriptions'))))
WHERE namespace = 'board_cache';
