# 0006 — Background jobs return an id; no queue, no database

**Status:** accepted
**Date:** 2026-09-06

## Context

A Wingman coding turn takes minutes and runs a build and test suite before it
finishes. It cannot block the loop, and its result arrives long after the turn
that requested it has ended.

The full solution is a job store: durable, restartable, queryable, surviving
restarts.

## Decision

`ToolOutcome::Started(JobId)` plus a `HashMap<JobId, JoinHandle>` in the main
loop. Nothing durable, nothing queryable, no database.

## Consequences

Jobs do not survive a process restart. A job in flight when IRA exits is lost,
and must be reported as lost rather than silently dropped.

This is sufficient for a single-user assistant on one machine where the process
runs as long as the desktop session does.

It also forces the proactive-speech question, which is unresolved: when a job
finishes, IRA must speak with no wake word for the first time. Current
recommendation is a completion tone immediately and a spoken summary at the next
entry to Idle.

## What would change this

Jobs routinely outliving the process, or more than a handful in flight at once.
Either would justify a durable store — as a separate decision, not by growing
this one.
