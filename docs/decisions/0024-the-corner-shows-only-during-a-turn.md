# 0024 — The corner shows only during a turn

**Status:** accepted
**Date:** 2026-11-16

## Context

[0023](0023-the-corners-show-what-she-is-doing.md) put two panels in the top
corners for as long as IRA runs. In use, both cost more than they gave.

The **top-right** panel showed the time, CPU, memory, battery, whether the
machine was online and the next reminders. All of it except the reminders is
already in the taskbar or Task Manager, and the panel sat over the minimise,
maximise and close buttons of whatever window was maximised under it. The
reminders are spoken when they come due.

The **top-left** panel is the one the orb cannot replace — what she heard, what
she said, how long it took, what she did — but it was there all day, mostly
saying *Idle*, *None yet* and *No actions yet* over a browser's tab strip. And
anything sharing the screen could read the last thing you asked her.

## Decision

The top-right panel is gone, and with it the system readings and the four
`windows-sys` features they needed.

The top-left panel opens when she wakes — listening, thinking, speaking or
asking — and closes eight seconds after she is idle again: long enough to read
the end of her reply. The rest of the time the corner is empty. `IRA_WIDGETS`
still hides it outright, and it still steps aside for a full-screen app.

## Consequences

Nothing on screen while she is idle except the orb, which was already the
signal that she is running.

The transcript is on screen only while there is a turn to show. Sharing the
screen while you talk to her still shows it.

The page at `http://127.0.0.1:8180` is the place to read a turn after the
panel has gone.

## What would change this

Wanting to glance at the last turn without talking to her. A hover on the orb
could bring the panel back for as long as the pointer stays there.
