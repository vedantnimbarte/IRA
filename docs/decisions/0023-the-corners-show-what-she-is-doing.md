# 0023 — The corners show what she is doing, in words

**Status:** accepted
**Date:** 2026-11-10

## Context

The orb ([0013](0013-the-orb-is-an-overlay-on-the-same-stream.md),
[0022](0022-the-orb-is-the-websites.md)) is a light. It says whether IRA is
idle, listening, thinking, speaking or asking, and nothing else. What she heard,
what she said, how long the turn took and what she did are all on the served
page, in a tab that is behind whatever you are working in.

0013 named the point where a lamp stops being enough: *"Wanting it to show
anything a light cannot — a transcript, a result, a menu."* This is that.

## Decision

Two panels open with the orb. The **top-left** one shows IRA: her state; the
last thing she heard and the start of her reply, or the question she is
waiting on; the last turn's time split into hearing, thinking and her voice,
marked when you cut in; the last tool she used and whether it worked; and the
model, where hearing happens, her voice, and how many MCP servers are live.
The **top-right** one shows the machine: the time and date, CPU and memory,
battery on a machine that has one, whether it is online, and the next two
reminders.

**The same kind of window as the orb.** A webview still cannot be transparent,
so these are layered windows drawn with tiny-skia and pushed with the orb's
`Canvas`, now shared. They are dark glass with the website's corner brackets,
nearly opaque, because what is underneath is usually text too.

**Click-through.** The orb is a button; these are not. `WS_EX_TRANSPARENT`
sends every click to the window underneath, so a panel in a corner never costs
you the close button or the tab strip it covers.

**Text is the one new dependency.** tiny-skia draws shapes, not text. swash is
pure Rust, has no build step, and reads variable fonts, so the panels are set
in Archivo at the website's widths rather than at one fixed instance. It needs
TrueType, so the settings page's Archivo moved from WOFF2 to the TTF, and both
read the one file.

**The same broadcast, and still not a screen.** They subscribe to the event
stream as the orb does. Subscribing is not watching: IRA does not tell the
model there is a screen because these are up, so she never claims to have put
the detail on one.

**Hidden from the settings window.** `IRA_WIDGETS` is a setting like the voice,
stored in `ira.local.db` and read on every frame, so hiding them takes effect
at once. It has a group of its own, **On screen**, since it is the one thing in
that pane that is not about what she hears or says.

**System readings are the cheapest native calls.** `GetSystemTimes` for CPU,
`GlobalMemoryStatusEx` for memory, `GetSystemPowerStatus` for the battery and
`InternetGetConnectedState` for online. CPU and memory are read once a second;
the network and the reminders every five seconds.

## Consequences

A transcript is now on screen whenever IRA runs, in a corner anyone looking at
the screen can read, and anything sharing the screen can see. That is the cost
of the feature, and it is why hiding them is one setting in the window.

swash and its dependencies on Windows only. The binary grows by the TTF, about
0.55 MB more than the WOFF2 it replaced.

The panels are a fixed size, which suits a fixed layout. A long reply is cut to
two lines with an ellipsis, and the page still has all of it.

They step out of the way, with the orb, while something has the whole screen
-- a full-screen app or game, or presentation mode -- using the same signal
Windows uses to hold back notifications. Screen sharing has no such signal, so
it is not covered; hiding them before sharing is on the user.

Every failure is a missing panel and never a broken IRA, on the same terms as
the orb: a font that will not load, a window that will not open, a reading that
fails (which shows as zero).

## What would change this

Wanting to interact with them — scrolling the reply, cancelling a reminder. At
that point they are a window, and the settings window shows how to have one.

Wanting them on another platform. The drawing is portable; the windows and the
system readings are not.
