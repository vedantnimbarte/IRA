# 0022 — The orb is the website's

**Status:** accepted
**Date:** 2026-11-10

## Context

[0013](0013-the-orb-is-an-overlay-on-the-same-stream.md) drew the orb as a pearl
sphere with iridescent light inside it. The landing page has since been
redesigned around a different orb: a dark core with a lit cyan rim, displaced by
noise, inside a dial. IRA was two things that looked like nothing to do with
each other — the page you read about her on, and the lamp in the corner of your
screen.

The website's orb is WebGL, and 0013 already measured that the overlay cannot be
a webview. Bringing the look over means drawing it again with tiny-skia.

## Decision

The overlay draws the website's orb, with the website's state table: the same
targets for how far the surface heaves, how fast, how much it ripples and how
bright it is, eased the same way. The window grows from 128 px to 160 px to fit
the dial around it.

**Cyan throughout, amber and grey for the exceptions.** The pearl orb said its
state with a palette per state. The website says it with motion, and keeps
colour for the two moments that are not hers: amber when she is asking you to
confirm something, or when you cut her off. Grey when she has stopped is kept
from 0013, because a lamp that glows after she exits is a claim she is running.

**A dark lens behind the dial.** The website's orb sits on a dark page, and an
overlay sits on whatever you have open. A dark disc behind it, fading out well
inside the window, makes it look the same on a white document as on a dark
wallpaper. The corners stay clear, which is the transparency claim the corner
test checks.

**The arc is time, not progress.** On the website, an arc fills as she speaks,
up to the length of a reply the page already knows. Here the reply streams, so
there is no length to fill to. The arc fills with how long she has been
talking, quickly at first and never lapping. When a turn ends with
`barged: true`, it stops where she stopped and turns amber, which is the
website's interrupt drawn on a lamp. That reads the `Turn` event, which was
already on the broadcast, so nothing behind the `Event` type changed.

**No HUD text.** 0013's reason for having no text still holds: this is a lamp.
The dial carries ticks and arcs, and no state name or timer.

## Consequences

One look across the page and the app, and the icon is still rendered from the
orb, so the taskbar matches it too. The icon draws the orb larger in its dial
than the window does, because at 16 px the window's proportions are a speck in
a dark disc.

Drawing it is more work per frame than the pearl was: the outline is sampled,
there are 140 dust points, and the bloom is blurred at half size. At 160 px and
30 frames a second, that is still not the expensive part of a process that runs
a neural VAD on every 32 ms of audio.

The ripple is a quarter of the website's. On a sphere shaded in 3D, it reads as
waves; on a flat outline every ripple is a spike, and at full strength she
becomes a starburst while she talks.

## What would change this

The website's orb changing again. Its targets live in `TARGETS` in the
website's `Orb.tsx`, and here in `Look::target`; the two tables are meant to
read the same.
