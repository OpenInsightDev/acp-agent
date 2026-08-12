# Issue: Readiness updates are ordered by completion, not launch generation

## Status

Resolved in `src/serve.rs`.

## Fix

Each launch receives a monotonic generation from the shared `AgentHealth` counter at factory creation.
`record_ok` and `record_failure` accept the outcome only when its generation is newer than the last recorded outcome.
An older successful connection that closes later is counted in the totals but can no longer overwrite a newer failed launch, so `GET /readyz` always reflects the most recent launch.

## Tests

`stale_launch_outcome_does_not_overwrite_newer_generation` completes two launches in reverse order.
`newer_launch_outcome_overwrites_older_generation` covers the successful follow-up launch.
`generations_are_monotonic_across_launches` covers the counter.
