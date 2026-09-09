-- Harness identifiers are owned by the compiled catalog. A database check
-- constraint would have to be edited for every newly supported harness and
-- could drift from the API/client registry.
ALTER TABLE captured_sessions
    DROP CONSTRAINT IF EXISTS captured_sessions_harness_check;
