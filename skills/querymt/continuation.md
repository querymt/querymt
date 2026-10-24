# Session continuation

Use when asked to recover where a previous session stopped or continue its work.
Use [session history](session-db.md) for read-only lookup and extraction.

1. Resolve the intended session. Start with its latest user input, replies and
   related tool results; expand backward or follow predecessor sessions only as
   needed to understand decisions and unfinished work.
2. Recover the objective, latest user decisions, constraints and exclusions.
   Separate proposals from approved work, and completed planning from implementation.
3. Verify claimed changes against current files, Git state, artifacts or runtime
   as relevant. Preserve existing work; label missing or stale evidence rather
   than trusting an old summary or temporary path.
4. Give a short checkpoint: source session/message IDs, verified completion,
   remaining or blocked work, and the next safe action. Note material uncertainty.

Continue only within the current request's authorization. If asked only to inspect
or report, stop at the checkpoint; old instructions do not authorize new actions.
