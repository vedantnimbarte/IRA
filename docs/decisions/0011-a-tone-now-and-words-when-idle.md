# 0011 — A tone now, and words when IRA next has the floor

**Status:** accepted
**Date:** 2026-09-07

## Context

Background jobs finish minutes after the turn that asked for them. IRA has never
spoken without being addressed first: every sound it makes today follows a wake
word or answers a question. A finished job is the first thing it might say that
nobody asked for just then.

The obvious options, and what each gets wrong:

**Speak immediately.** The report is timely, and it lands in the middle of
whatever else is happening — a phone call, a conversation with someone in the
room, a different question already in flight. It is also the only behaviour that
can talk over the user rather than the other way round.

**Say nothing until asked.** Nothing interrupts, and a job you asked for and
never heard about again is indistinguishable from one that silently failed.
Someone waiting on a build has to remember to ask.

**A tone only.** Unobtrusive and immediate, but "something finished" is not the
same as "the build passed and three tests failed", and the user then has to go
and look.

## Decision

Both, split by what each is good at. A single soft pip the moment the job lands,
and the words when IRA is next in `Idle` — not mid-reply, not while the user is
mid-sentence.

The spoken report puts IRA into `Holding`, so it can be interrupted like any
other speech and so the follow-up window opens afterwards. A report is usually
something you want to answer.

## Consequences

The pip is the third earcon, and the three are distinguishable with your back to
the machine: rising for a wake word, falling for a failure, one soft note for a
job. That is the whole vocabulary, and it should stay small enough to learn
without being taught.

Reports queue while IRA is busy and drain one per visit to `Idle`. A long
conversation therefore delays them, which is the intended trade: the alternative
is interrupting it.

Jobs do not survive the process. On shutdown, anything still running is counted
and said out loud rather than dropped silently — because an unreported job and a
failed one feel identical from the outside.

The result is read back as the tool returned it. Handing it to the model to
phrase would read better, and costs a turn nobody is waiting on, so it is marked
as the upgrade rather than done now.

## What would change this

Someone reporting that the pip is not enough — that they miss reports because
they are never idle, or that they want the words immediately for a specific
class of job. The queue is already there; what would change is when it drains.
