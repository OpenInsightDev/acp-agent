---
name: land
description: >-
  Land the current thread's changes in the acp-agent repository: verify them
  locally, commit on a branch, push that branch to origin, open or update the
  pull request, wait for CI, merge into main with a merge commit, and confirm the
  merged state. Invoke only when the user explicitly requests landing (Land
  Changes, `/land`, or wording like "land this" / "merge this"); not for
  reviewing, preparing, or merely running checks.
disable-model-invocation: true
metadata:
  delta-action: land
---

# Land acp-agent changes

Landing means the requested change is merged into `main` on
`github.com/OpenInsightDev/acp-agent` through a pull request, and `origin/main`
contains it.

## Preconditions

- `gh auth status` must show an authenticated account with `repo` scope. Otherwise stop and report.
- `git status --short --branch` in the thread's checkout. Everything uncommitted or untracked belongs to the change; if a file clearly does not belong to the request, ask before including it.
- Publish to `origin` only. Never push to `local`, the backlink to the original project checkout.

## Branch

The branch is the PR: reuse it while it is open rather than opening a second PR.

1. `git rev-parse --abbrev-ref HEAD`, then `gh pr list --head <branch> --state open --json number,url`.
2. Open PR → commit and push on that same branch.
3. No PR → `git switch -c land/<short-slug>` and work there. Switching keeps uncommitted changes, so the work travels with the new branch.

`git fetch origin` first when the branch may be behind; update it before opening
the PR only when GitHub reports it as conflicting.

## Local verification

Fix failures before committing. These are the same commands as
`.pre-commit-config.yaml` (fmt and clippy at pre-commit, test at pre-push),
`README.md` §Development, and the `check` and `test` jobs in
`.github/workflows/ci.yml`:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked --all-targets
```

## Commit

- Conventional Commits, matching history: `feat:`, `fix:`, `docs:`, `refactor:`, `chore:` (for example `chore(deps):`). Imperative subject, no trailing period.
- Keep `tests/<Feature>.md` and its paired `tests/<feature>.rs` in one commit and the implementation change in its own commit.
- Write the message yourself; add no generator footers or watermarks.

## Pull request

- `git push -u origin <branch>`, or plain `git push origin <branch>` when updating an open PR.
- Never force-push. Only when the user explicitly asks to rewrite history, use `git push --force-with-lease`.
- New PR: `gh pr create --base main --title "<subject>" --body "<what changed, how it was verified>"`. Include the verification commands and results, and the issue number when there is one.
- Open PR already exists: the push updated it. Refresh the body with `gh pr edit` only if the scope changed.

## CI

- `gh pr checks --watch`. Required checks from `.github/workflows/ci.yml`: `check`, `test (ubuntu-latest)`, `test (macos-15)`.
- Pending, failing, and missing checks do not count as satisfied.
- On failure: `gh run view <run-id> --log-failed`, fix, commit, push, watch again.
- `main` has no branch protection, so GitHub does not block a merge with red checks; enforcing this is the skill's job.

## Merge

- `gh pr view <number> --json mergeable,mergeStateStatus`.
- `CONFLICTING`: `git fetch origin`, `git merge origin/main` on the branch (keep the merge commit), resolve small and unambiguous conflicts, re-run local verification, push.
- Large divergence from upstream or unclear intent: stop and report the conflict instead of resolving it.
- Merge: `gh pr merge <number> --merge`. The repository merges PRs with merge commits; do not squash or rebase.
- Clean up: `git push origin --delete <branch>`, delete the local branch, then `git switch main` and update it from `origin/main`.

## Confirm

- `gh pr view <number> --json state,mergedAt,mergeCommit` reports `MERGED`.
- After `git fetch origin`, `git merge-base --is-ancestor <head-commit> origin/main` succeeds.
- Report the PR number and URL, the merge commit, and the CI result.

## When it cannot land

Stop and report that the change has not landed, naming the blocking step and its
output: failed or unavailable checks, a conflict you must not resolve alone,
missing authentication, or an unclear scope. Do not merge around a failing
check, bypass verification, or force-push.

Releasing is separate: `cargo release <major|minor|patch> -x` (root `AGENTS.md`)
only when the user asks for a release.
