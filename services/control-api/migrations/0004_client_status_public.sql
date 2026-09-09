-- Migration 0002 selects the Better Auth schema for its connection. SQLx can
-- reuse that connection for later migrations, so explicitly move the Control
-- API-owned table back to public.
ALTER TABLE IF EXISTS auth.client_status SET SCHEMA public;
