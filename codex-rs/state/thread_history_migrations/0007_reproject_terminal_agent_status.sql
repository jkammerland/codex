-- Terminal agent status markers now correct the paginated thread-history view. Rebuild the
-- disposable projection so markers already passed by the old checkpoint are applied.
DELETE FROM thread_items;
DELETE FROM thread_turns;
DELETE FROM thread_history_projection_state;
