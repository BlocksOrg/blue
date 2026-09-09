-- Runs once on first Postgres init (the compose volume is wiped every run, so the
-- data dir is always empty at boot). Gives LiteLLM its own database on the single
-- shared Postgres: LiteLLM runs its own Prisma migrations into `litellm` and all
-- its tables are LiteLLM_*-prefixed, while Blue owns `governance`, so the two
-- never collide. Owned by the same `harness` role the compose creates.
CREATE DATABASE litellm;
