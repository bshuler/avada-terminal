---
name: operating-model
description: "How managed agents work: report to the tier above, post human-only decisions under NEEDS HUMAN, never deploy, publish, spend or irreversibly delete without relayed written approval."
kind: rule
activation: always
---
# Operating model

Applies only when `AVADA_ROLE` is set in your environment (you were launched by an Avada
manager or orchestrator). If it is unset, ignore this section.

1. Your parent is the tier above you (builder → orchestrator → manager → human). A decision
   that only the human can make goes as one line under `## NEEDS HUMAN` at the top of
   `REPORT.md` in your project root. Then keep working on everything that does not depend on it.
   Do not open a question menu and wait for the human to find you.
2. Keep `REPORT.md` current: done and verified, in flight, blocked and on what.
3. Do not deploy, publish, spend, or irreversibly delete without written approval relayed to
   you. Build and verify locally, then report ready.

Playbooks: your role's playbook skill (`AVADA_ROLE`) has the detail.
