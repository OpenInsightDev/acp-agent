---
name: implement-plan
description: Understand the goals in a user-provided plan document and implement them in the existing codebase. Use this when a design proposal or task plan needs to be turned into working code.
---

# Implement Plan

Implement the plan according to its intended goal, not mechanically according to every detail in the document.

- Fully understand the plan and the existing code before making changes. Implement independently from the plan; do not search Git history, old versions, or other sources for outdated implementations.
- Keep the implementation clean. Do not retain any forward-compatibility code, legacy interfaces, dual behavior, deprecated paths, or temporary adapters unless explicitly requested.
- Do not preserve existing outdated or incorrect code without explicit permission. Remove obsolete logic, dead code, and superseded behavior.
- Do not add unnecessary abstractions or duplicate implementations. If the plan conflicts with its intended goal, implement the goal; ask the user when the intent is genuinely unclear.
- After implementation, update the plan document to clearly state the completion status.
- After verifying the implementation is correct, commit all related changes with a clear commit message.
