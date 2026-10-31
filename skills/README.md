# Project skills

## QueryMT

[querymt](querymt/SKILL.md) supports read-only session-history inspection and
failed-delegation recovery. It brings the local `session-db` and
`delegation-recovery` workflows together under one entry point, with separate
references loaded only when needed. It does not require those local skills.

Use it for explicit history requests or targeted delegation-failure recovery,
not for browsing unrelated sessions or modifying the session database.

## QueryMT development

[querymt-dev](querymt-dev/SKILL.md) supports autonomous QueryMT backend development
and validation on Linux and macOS. UI work belongs to its separate repository/workflow:
report needed UI follow-up, do not execute it here.
Load the existing qmacp skill for client operation. This skill supplies only the
availability checks and acceptance focus, not another client manual.

Read only task-relevant references. This is a local development workflow, not a
mandatory full-suite pipeline.
