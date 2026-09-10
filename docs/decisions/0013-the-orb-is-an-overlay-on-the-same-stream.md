# 0013 — The orb is an overlay on the same event stream

**Status:** accepted
**Date:** 2026-09-10

## Context

[0009](0009-the-screen-is-a-served-page.md) made the screen a page served on
loopback and named its own cost: *"a tab you leave open rather than an overlay
that appears when IRA speaks. That is a real loss of immediacy."* Its "what
would change this" section is precise about the argument that would extend it —
wanting something that appears over other windows — and about the shape of the
answer: *"an overlay consumes exactly the same event stream. The serialised
`Event` type is the contract, and a different front end is a different consumer
of it, not a rewrite of anything behind it."*

That argument arrived. There is no way to tell from another application whether
IRA is running, listening, or talking. The tab answers all three and is behind
whatever you are working in.

This does not supersede 0009. The page stays exactly as it is; the orb is the
second consumer 0009 anticipated, and nothing behind the `Event` type changed.

## Decision

A transparent, always-on-top window in the bottom-left corner, 128 px square: a
pearl sphere with iridescent light moving inside it — colour drifting under the
surface, pale ribbons flowing across and crossing each other. It reads the same
broadcast the page's event stream reads, in process.

State is carried by the palette and by how fast everything moves. An orb like
this has no other vocabulary: there is no text on it and no meter, so cyan and
quick is listening, magenta and quicker is speaking, warm and almost still is a
question waiting for an answer. The colours are sampled from the reference
rather than invented.

**It is drawn, not rendered.** The first implementation was a webview pointed at
an `/orb` route serving the globe as CSS. That is the cheaper idea by a long way
— no rasteriser, and one description of the orb rather than two — and it does
not work. A window hosting a windowed WebView2 cannot be made transparent on
Windows 11, and an overlay that is not transparent is a white square sitting on
top of your work. Five arrangements were measured, each by capturing the screen
and comparing it against the desktop behind the window:

| Arrangement | Result |
|---|---|
| `with_transparent(true)` (tao's `DwmEnableBlurBehindWindow`) | white square |
| `DwmExtendFrameIntoClientArea`, -1 margins | `S_OK`, white square |
| `WS_EX_NOREDIRECTIONBITMAP` | black square |
| `WS_EX_LAYERED` + black colour key | every pixel still differed |
| the above stripped back to a minimal window | 0.0% see-through |

Serving the page with a red background showed red, so the webview was never the
problem. wry 0.55 has no composition hosting anywhere in its source, so the
webview renders through a child HWND with no per-pixel alpha to give. Nothing
above it can composite what it never produced.

`UpdateLayeredWindow` takes a premultiplied ARGB bitmap and is per-pixel alpha
by definition. The orb is rasterised with tiny-skia — pure Rust, no C
dependency, no build step — and pushed to the window each frame.

**It owns its window.** `UpdateLayeredWindow` paints a window as one bitmap and
refuses a window that has a frame, and a decorations-off tao window keeps
`WS_BORDER | WS_THICKFRAME` in its style. Every call with a source DC returned
`ERROR_INVALID_PARAMETER` — including one using the screen itself as the source
— while the same call with no source at all succeeded. A `WS_POPUP` window,
created `WS_EX_LAYERED` from birth, is what the API wants. That is ~120 lines of
Win32, and it removed both `tao` and `wry`: the whole overlay is now two
dependencies, one of which is the Windows API.

**A `Speaking { on }` event, not a fifth state.** `Holding` covers thinking and
speaking as one because barge-in must be armed across both, and 0001's
single-process guarantee is what makes that work. Splitting the state to light a
lamp would put a real guarantee at risk for a cosmetic one. `Speaking` rides
alongside, is emitted on transitions of the audio queue, and no state machine
reads it.

**The orb does not count as a watcher.** `watchers()` gates the system prompt's
screen clause. An orb that counted would have IRA saying she had put the detail
on screen with no page open — the lie P1 removed. It subscribes to the broadcast
rather than connecting to the socket, and subscribing is not watching.

**Clicking it presses talk.** The README says binding the talk control to a real
hotkey is the OS's job. A lamp already floating above every window is the
cheapest thing that has ever been closer to hand. A layered window hit-tests by
alpha, so a click on the clear corners goes to whatever is behind it — the
click-through an overlay needs, for free.

**Windows only, for now.** The window, the layered bitmap and the message loop
are all Win32, declared under `[target.'cfg(windows)'.dependencies]` so no other
platform compiles them and the Linux CI job does not grow a dependency for a
feature it does not get.

## Consequences

Two dependencies on one platform, and no webview: no WebView2 runtime to be
present, no second build pipeline, no HTML at all. The orb is a `paint` function
over a `Pixmap`, which is also why it can be tested without a window —
`the_corners_are_clear_in_every_state` rasterises a frame and reads the alpha
back, which is the entire transparency claim as an assertion, and
`the_sphere_has_colour_in_it` catches the orb going white -- which is what a
broken blur, an over-clipping mask or a grey palette all look like, and all
three still paint a perfectly convincing sphere.

Every failure is a missing light and never a broken IRA, on the same terms
`ui.rs` sets: no display, a window that will not open, a bitmap that will not
allocate — all logged and swallowed. A failing `UpdateLayeredWindow` is reported
once rather than thirty times a second. `IRA_ORB=off` disables it.

macOS and Linux get nothing, and the drawing is portable but the window is not.
The README already lists macOS as unverified; this does not make that worse, and
does not fix it.

## What would change this

Wanting the orb on another platform. The `paint` function is portable; a macOS
or Wayland overlay is a new window and the same rasteriser, not a new orb.

Wanting it to show anything a light cannot — a transcript, a result, a menu. At
that point it stops being a lamp, and a rasteriser drawing shapes stops being
the right tool.
