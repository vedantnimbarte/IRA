//! The orb: a transparent, always-on-top globe showing IRA's state.
//!
//! [decisions/0013](../docs/decisions/0013-the-orb-is-an-overlay-on-the-same-stream.md).
//! ADR 0009 kept the screen a served page and named the one thing that would
//! add a window: wanting an overlay that appears over other work rather than a
//! tab you visit. This is that, and it consumes the same events the page does --
//! in process, off the same broadcast, rather than over a socket to itself.
//!
//! **Why this is drawn rather than rendered.** The first version was a webview
//! pointed at a route serving the orb as CSS, which is the cheaper idea by a
//! long way: no rasteriser, and one description of the orb instead of two. It
//! does not work. A window hosting a windowed WebView2 cannot be made
//! transparent on Windows 11, and an overlay that is not transparent is a white
//! square sitting on top of your work. Five arrangements were measured, all by
//! capturing the screen and comparing it against the desktop behind the window:
//!
//! | Arrangement | Result |
//! |---|---|
//! | `with_transparent(true)` (tao's `DwmEnableBlurBehindWindow`) | white square |
//! | `DwmExtendFrameIntoClientArea`, -1 margins | `S_OK`, white square |
//! | `WS_EX_NOREDIRECTIONBITMAP` | black square |
//! | `WS_EX_LAYERED` + black colour key | every pixel still differed |
//! | all of the above stripped back to a minimal window | 0.0% see-through |
//!
//! Serving the page with a red background showed red, so the webview was never
//! the problem. wry 0.55 has no composition hosting -- no `DirectComposition`,
//! no `CoreWebView2CompositionController` anywhere in its source -- so the
//! webview renders through a child HWND that has no per-pixel alpha to give.
//! Nothing above it can composite what it never produced.
//!
//! `UpdateLayeredWindow` takes a premultiplied ARGB bitmap and is per-pixel
//! alpha by definition. It also hit-tests by alpha, so clicks land on the orb
//! and pass straight through the transparent corners to whatever is behind --
//! which the webview version had to fake by keeping the window small.
//!
//! Windows only, for now. The event loop runs on a spawned thread via
//! `with_any_thread`, a Windows affordance, and it is what keeps the voice loop
//! on the main thread it has always had. macOS would need that loop moved,
//! which means restructuring `main.rs` -- the one file where a mistake is a
//! conversation that does not happen.
//!
//! The whole module is Windows-only -- `main.rs` does not declare it elsewhere,
//! and tiny-skia is a Windows-only dependency -- which is why nothing inside it
//! is individually gated. On any other platform none of this is compiled, and
//! `spawn` is simply never called.
//!
//! Nothing here may affect the loop, exactly as in `ui.rs`. Every failure is
//! logged and swallowed: no display, a window that will not open, a bitmap that
//! will not allocate. IRA still answers questions.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Logical size of the window: the orb, and the dial around it.
const SIZE: f64 = 160.0;
/// Gap from the corner of the work area, so it clears a taskbar.
const MARGIN: f64 = 24.0;
/// Redraws a second. The orb is 160 px of gradient; this is not the expensive
/// part of a process that runs a neural VAD on every 32 ms of audio.
const FPS: u64 = 30;
/// Where the gear sits, in logical pixels from the centre of the window, and
/// how big it is. Up and to the right: the orb lives in the bottom-left corner
/// of the screen, so that is the side with room and the side your pointer
/// arrives from. On the dial's rim rather than the orb, so it covers a tick
/// mark and not her.
const GEAR_AT: (f32, f32) = (52.0, -52.0);
const GEAR_R: f32 = 12.0;
/// How long the pointer has to stay on another monitor before the orb goes
/// after it: long enough that sweeping across a screen on the way to another
/// does not drag her through it.
const FOLLOW: Duration = Duration::from_millis(500);
/// How long the orb takes to fade out of one monitor, and again into the next.
const MOVE_FADE: f32 = 0.15;

/// The monitor the orb sits on, as an `HMONITOR`; zero until it is placed.
/// The corner widget reads it, so the panel opens on the same screen.
static ON: AtomicIsize = AtomicIsize::new(0);

/// The monitor the orb is on, or null before it has been placed.
pub(crate) fn monitor() -> windows_sys::Win32::Graphics::Gdi::HMONITOR {
    ON.load(Ordering::Relaxed) as _
}

/// What the orb is showing. Not `main::State`: the loop's `Holding` covers
/// thinking and speaking as one -- barge-in has to be armed across both -- and
/// out here they are the two things you most want to tell apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Look {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Confirming,
    /// IRA is not running, or has stopped. A lamp still glowing after she exits
    /// is a claim she is running.
    Dead,
}

impl Look {
    /// The state names `main::state_name` emits. Anything else leaves the orb
    /// as it was, because a light that guesses is worse than one that waits.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "idle" => Look::Idle,
            "listening" => Look::Listening,
            "holding" => Look::Thinking,
            "confirming" => Look::Confirming,
            _ => return None,
        })
    }

    /// Where this state pulls the drawing. Every value eases toward its row
    /// rather than snapping. The same table as `TARGETS` in the website's
    /// `Orb.tsx`, which this orb is drawn to match: cyan throughout, with state
    /// carried by how the surface moves, amber when she is asking you something
    /// and grey when she has stopped.
    fn target(self) -> Target {
        match self {
            //                      amp,  speed, wave, glow, size, busy, accent
            Look::Idle => Target::new(0.10, 0.16, 0.0, 0.78, 1.0, 0.0, CYAN),
            Look::Listening => Target::new(0.15, 0.34, 0.03, 0.95, 1.05, 0.07, CYAN),
            Look::Thinking => Target::new(0.23, 1.6, 0.0, 1.15, 1.0, 0.24, CYAN),
            // The website's ripple is 0.3 on a sphere shaded in 3D. On a flat
            // outline every ripple is a spike, and at that size she becomes a
            // starburst, so it is a quarter of that.
            Look::Speaking => Target::new(0.16, 0.5, 0.08, 1.35, 1.06, 0.0, CYAN),
            Look::Confirming => Target::new(0.08, 0.2, 0.0, 1.0, 1.0, 0.0, AMBER),
            Look::Dead => Target::new(0.0, 0.0, 0.0, 0.3, 0.9, 0.0, GREY),
        }
    }
}

/// The website's accent, `--cyan`.
const CYAN: [f32; 3] = [47.0, 224.0, 208.0];
/// `--ice`: the working arc and the wake shock, a paler cyan.
const ICE: [f32; 3] = [159.0, 243.0, 234.0];
/// `--amber`, the "you took control" colour: asking to confirm, and a cut.
const AMBER: [f32; 3] = [232.0, 180.0, 85.0];
/// Switched off, not broken.
const GREY: [f32; 3] = [138.0, 150.0, 156.0];
/// The core of the sphere and the lens behind the dial: the website's page.
const CORE: [f32; 3] = [2.0, 9.0, 11.0];
const VOID: [f32; 3] = [5.0, 8.0, 10.0];

/// One row of the state table.
#[derive(Clone, Copy)]
struct Target {
    /// How far the surface heaves, as a fraction of the radius.
    amp: f32,
    /// How fast it heaves.
    speed: f32,
    /// The ripples that run across it while she talks.
    wave: f32,
    /// Brightness of the rim, the dust and the bloom.
    glow: f32,
    size: f32,
    /// The working arc's length, as a fraction of the dial. Zero is none.
    busy: f32,
    accent: [f32; 3],
}

impl Target {
    const fn new(amp: f32, speed: f32, wave: f32, glow: f32, size: f32, busy: f32, accent: [f32; 3]) -> Self {
        Self { amp, speed, wave, glow, size, busy, accent }
    }
}

/// How long the arc lingers once she stops, so a cut stays on screen long
/// enough to be seen and a pause between sentences does not blink it.
const SAID_FADE: f32 = 2.5;

/// Everything one frame is drawn from, eased toward the current look's
/// [`Target`] a step at a time.
#[derive(Clone, Copy, Debug)]
struct Frame {
    look: Look,
    /// Seconds since the orb opened: what the dial, the dust and the working
    /// arc turn by.
    t: f32,
    /// The surface's own clock, advanced at the state's speed. Integrated
    /// rather than `t * speed`, so a change of speed changes the pace and not
    /// the position -- which would otherwise jump on every state change.
    phase: f32,
    amp: f32,
    speed: f32,
    wave: f32,
    glow: f32,
    size: f32,
    busy: f32,
    accent: [f32; 3],
    /// How much of the dial the speaking arc has swept, 0 to 1.
    said: f32,
    said_alpha: f32,
    /// She was interrupted: the arc stops where she did, in amber.
    cut: bool,
    /// The ring fired on waking and on a cut, 0 to 1; above 1 is none.
    shock: f32,
}

impl Frame {
    /// A look fully arrived, with nothing in flight. What the tests and the
    /// icon draw.
    fn at_rest(look: Look, t: f32) -> Self {
        let g = look.target();
        Self {
            look,
            t,
            phase: t * g.speed,
            amp: g.amp,
            speed: g.speed,
            wave: g.wave,
            glow: g.glow,
            size: g.size,
            busy: g.busy,
            accent: g.accent,
            said: 0.0,
            said_alpha: 0.0,
            cut: false,
            shock: 2.0,
        }
    }

    /// Advances `dt` seconds toward `look`. `barged` is whether a turn has just
    /// ended with her interrupted; `heard` is how much of the reply in progress
    /// has been played, over how much both engines have made so far.
    fn step(&mut self, look: Look, dt: f32, barged: bool, heard: f32) {
        if look != self.look {
            // Idle to listening is the wake word firing, or a press of the orb.
            if self.look == Look::Idle && look == Look::Listening {
                self.shock = 0.0;
            }
            // A new reply starts a new arc -- but a pause between two
            // sentences of the same one, with the arc still showing, does not.
            if look == Look::Speaking && self.said_alpha < 0.05 {
                self.said = 0.0;
                self.cut = false;
            }
            self.look = look;
        }
        if barged {
            self.cut = true;
            self.shock = 0.0;
        }

        // The website's easing: quick enough to read as a change of state,
        // slow enough that nothing snaps.
        let g = look.target();
        let k = 1.0 - (-dt * 7.0).exp();
        let ease = |v: &mut f32, to: f32| *v += (to - *v) * k;
        ease(&mut self.amp, g.amp);
        ease(&mut self.speed, g.speed);
        ease(&mut self.wave, g.wave);
        ease(&mut self.glow, g.glow);
        ease(&mut self.size, g.size);
        ease(&mut self.busy, g.busy);
        for (c, to) in self.accent.iter_mut().zip(g.accent) {
            ease(c, to);
        }

        self.t += dt;
        self.phase += dt * self.speed;
        // How much of the reply she has said. It can step back when a sentence
        // lands faster than she speaks it -- with the total unknown until she
        // has finished, that is the honest picture -- and the easing turns the
        // step into a glide. A cut freezes it where she stopped.
        if !self.cut {
            ease(&mut self.said, heard.clamp(0.0, 1.0));
        }
        if look == Look::Speaking {
            self.said_alpha = (self.said_alpha + dt * 4.0).min(1.0);
        } else {
            self.said_alpha = (self.said_alpha - dt / SAID_FADE).max(0.0);
        }
        if self.shock <= 1.0 {
            self.shock += dt * 1.6;
        }
    }
}

/// What the drawing thread reads and the event stream writes. Atomics rather
/// than a lock: the painter must never wait on the loop, and two scalars do not
/// need a mutex to be read consistently enough for a lamp.
#[derive(Default)]
struct Shared {
    /// A `Look` discriminant, without `Speaking` or `Dead` -- those are decided
    /// from the two flags below.
    state: AtomicU8,
    speaking: AtomicBool,
    live: AtomicBool,
    /// A turn just ended with her interrupted. Set by the feed, taken -- read
    /// and cleared -- by the painter, so each cut is drawn once.
    barged: AtomicBool,
    /// The last `said` event as a fraction heard, 0 to 1, in an `f32`'s bits.
    heard: AtomicU32,
}

impl Shared {
    fn look(&self) -> Look {
        if !self.live.load(Ordering::Relaxed) {
            return Look::Dead;
        }
        if self.speaking.load(Ordering::Relaxed) {
            return Look::Speaking;
        }
        match self.state.load(Ordering::Relaxed) {
            1 => Look::Listening,
            2 => Look::Thinking,
            3 => Look::Confirming,
            _ => Look::Idle,
        }
    }

    fn set(&self, look: Look) {
        self.state.store(
            match look {
                Look::Listening => 1,
                Look::Thinking => 2,
                Look::Confirming => 3,
                _ => 0,
            },
            Ordering::Relaxed,
        );
    }
}

/// Opens the orb over everything, on its own thread.
///
/// `IRA_ORB=off` disables it.
pub fn spawn(ui: &crate::ui::Ui) {
    if std::env::var("IRA_ORB").unwrap_or_default() == "off" {
        tracing::info!("orb disabled");
        return;
    }

    let shared = Arc::new(Shared::default());
    shared.live.store(true, Ordering::Relaxed);

    // The same events the page gets, read here rather than sent: subscribing to
    // a broadcast is not a watcher, so `ui.watchers()` stays honest and IRA
    // never claims to have put detail on a lamp.
    let mut events = ui.subscribe();
    let feed = shared.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let json = match events.recv().await {
                Ok(json) => json,
                // Fell behind and missed some events. That is a reason to read
                // on, not to go grey: IRA is still running, and the next state
                // event puts the orb right. Treating it as the end, as this
                // loop once did, left a running IRA looking stopped for good.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(v) => match v["kind"].as_str() {
                    Some("state") => {
                        if let Some(look) = v["name"].as_str().and_then(Look::from_name) {
                            feed.set(look);
                        }
                    }
                    Some("speaking") => {
                        feed.speaking.store(v["on"].as_bool().unwrap_or(false), Ordering::Relaxed);
                    }
                    Some("said") => {
                        let n = |k: &str| v[k].as_u64().unwrap_or(0) as f32;
                        let heard = if n("voiced_ms") > 0.0 { n("played_ms") / n("voiced_ms") } else { 0.0 };
                        feed.heard.store(heard.min(1.0).to_bits(), Ordering::Relaxed);
                    }
                    Some("turn") if v["barged"].as_bool() == Some(true) => {
                        feed.barged.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                },
                Err(e) => tracing::debug!("orb could not read an event: {e}"),
            }
        }
        // The sender is gone, which means IRA is on her way out.
        feed.live.store(false, Ordering::Relaxed);
    });

    let ui = ui.clone();
    std::thread::spawn(move || {
        if let Err(e) = run(shared, ui) {
            // A window that will not open is a missing light, not a broken IRA.
            tracing::error!("orb unavailable: {e}");
        }
    });
}

// Everything the window procedure needs. There is one orb per process and it
// lives on the thread that owns the window, so a thread local is the whole of
// it: no pointer to smuggle through `GWLP_USERDATA` and get wrong.
thread_local! {
    static ORB: std::cell::RefCell<Option<Orb>> = const { std::cell::RefCell::new(None) };
}

struct Orb {
    canvas: Canvas,
    shared: Arc<Shared>,
    ui: crate::ui::Ui,
    /// What is on screen, eased a step toward the current look each frame.
    frame: Frame,
    /// When the last frame was drawn. Timer messages arrive late under load,
    /// and easing by the time that actually passed keeps the motion steady.
    last: std::time::Instant,
    scale: f32,
    /// Whether the pointer is over the orb. The gear only exists while it is:
    /// a lamp with a permanent button on it is a widget, and this is meant to
    /// be something you stop noticing.
    hovered: bool,
    /// Eases the gear in and out, so it arrives rather than appears. Runs 0 to
    /// 1 in `GEAR_FADE` seconds and is what `paint` actually draws from.
    reveal: f32,
    /// The settings window, kept alive as long as it is open. Dropping it
    /// closes the webview, so this is also how "already open" is answered.
    settings: Option<Settings>,
    /// A full-screen app or a presentation has the screen, so the orb is out
    /// of its way; see `hud::screen_taken`. Checked about once a second.
    hidden: bool,
    frames: u32,
    /// The monitor the pointer is on, and since when: the orb follows it to
    /// another screen once it has stayed there for [`FOLLOW`].
    pointer: (isize, Instant),
    /// 1 on screen, 0 faded out while moving to the pointer's monitor.
    shown: f32,
}

/// The window class name, UTF-16 because that is what `RegisterClassW` reads.
const CLASS: &[u16] = &[b'I' as u16, b'R' as u16, b'A' as u16, b'O' as u16, b'r' as u16, b'b' as u16, 0];

/// The frame timer's id. Any non-zero value; there is only ever one.
const TIMER: usize = 1;

fn run(shared: Arc<Shared>, ui: crate::ui::Ui) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, LoadCursorW, RegisterClassW, SetTimer,
        ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, IDC_ARROW, MSG, SW_SHOWNOACTIVATE,
        WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    // The window is created layered, rather than made layered afterwards, and
    // this is the whole reason the orb owns its window instead of borrowing one
    // from a windowing crate.
    //
    // `UpdateLayeredWindow` paints a window as a single bitmap and will not do
    // that for a window with a frame -- and a decorations-off tao window keeps
    // `WS_BORDER | WS_THICKFRAME` in its style. Measured: with a source DC every
    // call returned ERROR_INVALID_PARAMETER, including one using the screen
    // itself as the source, while the same call with no source at all
    // succeeded. `WS_POPUP` from birth, with no non-client area to account for,
    // is what it wants.
    //
    // SAFETY: a class registered with a static name and a window procedure of
    // the required signature, then a window created from it. Every handle is
    // checked before it is used.
    //
    // Per-monitor DPI aware first, so the orb is drawn at the display's real
    // resolution and told when its scale changes -- and so is the settings
    // window, which this thread also opens.
    crate::hud::per_monitor_dpi();
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance as _,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            // No brush: a layered window's pixels all come from the bitmap, and
            // a brush here is the white flash before the first frame.
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS.as_ptr(),
        };
        // Zero means "already registered", which is not an error worth telling
        // apart: the class is identical either way.
        RegisterClassW(&class);

        let hwnd: HWND = CreateWindowExW(
            // Layered from birth. A tool window so it is not in alt-tab, topmost
            // so it is over your work, and NOACTIVATE so clicking it never takes
            // focus from whatever you were typing in.
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            CLASS.as_ptr(),
            CLASS.as_ptr(),
            WS_POPUP,
            0,
            0,
            SIZE as i32,
            SIZE as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance as _,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return Err(anyhow::anyhow!("orb window would not open"));
        }

        // Now that there is a window there is a monitor, and so a scale.
        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let side = (SIZE as f32 * scale) as u32;
        let canvas = Canvas::new(hwnd, side, side)?;
        settle(hwnd, side, scale);

        ORB.with(|cell| {
            *cell.borrow_mut() = Some(Orb {
                canvas,
                shared,
                ui,
                frame: Frame::at_rest(Look::Idle, 0.0),
                last: std::time::Instant::now(),
                scale,
                hovered: false,
                reveal: 0.0,
                settings: None,
                hidden: false,
                frames: 0,
                pointer: (ON.load(Ordering::Relaxed), Instant::now()),
                shown: 1.0,
            });
        });

        // Painted before it is shown. A layered window has no pixels at all
        // until the first frame lands, so there is nothing to flash.
        frame();
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, TIMER, (1000 / FPS) as u32, None);

        tracing::info!("orb up");
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Draws one frame, if there is an orb on this thread to draw -- or hides it
/// while something has the whole screen, and brings it back after.
///
/// Also where the orb follows the pointer to another monitor: it fades out,
/// moves, and fades back in. The move is made after the borrow is dropped,
/// because a monitor with another scale answers it with `WM_DPICHANGED`, and
/// that handler needs the orb to rebuild its canvas.
fn frame() {
    let moving = ORB.with(|cell| draw_frame(cell.borrow_mut().as_mut()?));
    if let Some((hwnd, monitor, side, scale)) = moving {
        settle_on(hwnd, monitor, side, scale);
    }
}

/// One frame of [`frame`]; returns where the orb should move to, if anywhere.
fn draw_frame(
    orb: &mut Orb,
) -> Option<(windows_sys::Win32::Foundation::HWND, windows_sys::Win32::Graphics::Gdi::HMONITOR, u32, f32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE};
    orb.frames = orb.frames.wrapping_add(1);
    if orb.frames.is_multiple_of(FPS as u32) {
        let taken = crate::hud::screen_taken();
        if taken != orb.hidden {
            orb.hidden = taken;
            // SAFETY: the window this thread created.
            unsafe { ShowWindow(orb.canvas.hwnd, if taken { SW_HIDE } else { SW_SHOWNOACTIVATE }) };
        }
    }
    if orb.hidden {
        // Nobody can see it, so it costs nothing either.
        orb.last = std::time::Instant::now();
        return None;
    }
    let look = orb.shared.look();
    let barged = orb.shared.barged.swap(false, Ordering::Relaxed);
    let now = std::time::Instant::now();
    // Capped, so a frame after the machine wakes from sleep eases
    // rather than teleporting.
    let dt = now.duration_since(orb.last).as_secs_f32().min(0.1);
    orb.last = now;
    let heard = f32::from_bits(orb.shared.heard.load(Ordering::Relaxed));
    orb.frame.step(look, dt, barged, heard);
    // A frame's worth of fade, toward wherever the pointer is.
    let step = 1.0 / (FPS as f32 * GEAR_FADE);
    let target = if orb.hovered { 1.0 } else { 0.0 };
    orb.reveal = if orb.reveal < target {
        (orb.reveal + step).min(target)
    } else {
        (orb.reveal - step).max(target)
    };
    // Off to the pointer's monitor once it has settled there; back in
    // if it comes home before the fade is done.
    let pointer = pointer_monitor();
    if pointer != orb.pointer.0 {
        orb.pointer = (pointer, now);
    }
    let on = ON.load(Ordering::Relaxed);
    let leaving = pointer != 0 && pointer != on && orb.pointer.1.elapsed() >= FOLLOW;
    let fade = dt / MOVE_FADE;
    orb.shown = if leaving { (orb.shown - fade).max(0.0) } else { (orb.shown + fade).min(1.0) };
    let eased = orb.shown * orb.shown * (3.0 - 2.0 * orb.shown);
    orb.canvas.alpha = (eased * 255.0).round() as u8;
    orb.canvas.draw(&orb.frame, orb.scale, orb.reveal);
    (leaving && orb.shown == 0.0).then_some((orb.canvas.hwnd, pointer as _, orb.canvas.width, orb.scale))
}

/// The monitor the pointer is on, as an `HMONITOR`, or zero if there is no
/// pointer to ask about.
fn pointer_monitor() -> isize {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    // SAFETY: a POINT that outlives the call.
    unsafe {
        let mut at = std::mem::zeroed();
        if GetCursorPos(&mut at) == 0 {
            return 0;
        }
        MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST) as isize
    }
}

/// How long the gear takes to arrive, and to leave.
const GEAR_FADE: f32 = 0.16;

/// Whether a click at this point, in window pixels, landed on the gear.
///
/// The same numbers `paint` draws it from, so the thing you can press is the
/// thing you can see. A little larger than it looks: a 12 px target is small,
/// and the cost of being generous is a click near the gear opening settings
/// rather than taking the floor, which is recoverable either way.
fn on_gear(x: i32, y: i32, side: f32, scale: f32) -> bool {
    let (cx, cy) = (side / 2.0, side / 2.0);
    let gx = cx + GEAR_AT.0 * scale;
    let gy = cy + GEAR_AT.1 * scale;
    let (dx, dy) = (x as f32 - gx, y as f32 - gy);
    (dx * dx + dy * dy).sqrt() <= GEAR_R * scale * 1.25
}

unsafe extern "system" fn wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE};
    // WM_MOUSELEAVE is filed under Controls rather than WindowsAndMessaging,
    // which is where TrackMouseEvent's documentation would have you look.
    use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN,
        WM_MOUSEMOVE, WM_SETTINGCHANGE, WM_TIMER,
    };
    match msg {
        // The display's scale changed: a new canvas at the new size, then the
        // corner again, since the margin scales too. Settled outside the
        // borrow, because moving a window sends messages back here.
        WM_DPICHANGED => {
            let scale = (wparam & 0xffff) as f32 / 96.0;
            let side = (SIZE as f32 * scale) as u32;
            let rebuilt = ORB.with(|cell| {
                let mut cell = cell.try_borrow_mut().ok()?;
                let orb = cell.as_mut()?;
                match Canvas::new(hwnd, side, side) {
                    Ok(canvas) => {
                        orb.canvas = canvas;
                        orb.scale = scale;
                        Some(())
                    }
                    Err(e) => {
                        tracing::error!("orb could not follow the new scale: {e}");
                        None
                    }
                }
            });
            if rebuilt.is_some() {
                settle(hwnd, side, scale);
                frame();
            }
            0
        }
        // A resolution change, or the taskbar moving: the corner moved too.
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            let placed = ORB.with(|cell| {
                cell.try_borrow().ok().and_then(|c| c.as_ref().map(|o| (o.canvas.width, o.scale)))
            });
            if let Some((side, scale)) = placed {
                settle(hwnd, side, scale);
            }
            // SAFETY: forwarding the message exactly as it arrived.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_TIMER => {
            frame();
            0
        }
        // Windows sends WM_MOUSEMOVE while the pointer is over the window but
        // nothing at all when it leaves, so leaving has to be asked for --
        // once, each time it arrives. Without this the gear stays out forever
        // after the first time you pass over the orb.
        WM_MOUSEMOVE => {
            ORB.with(|cell| {
                if let Some(orb) = cell.borrow_mut().as_mut() {
                    if !orb.hovered {
                        orb.hovered = true;
                        let mut track = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        // SAFETY: a TRACKMOUSEEVENT with its own size set, for
                        // the window this message is for.
                        unsafe { TrackMouseEvent(&mut track) };
                    }
                }
            });
            0
        }
        WM_MOUSELEAVE => {
            ORB.with(|cell| {
                if let Some(orb) = cell.borrow_mut().as_mut() {
                    orb.hovered = false;
                }
            });
            0
        }
        // A press of the orb is the talk control: it takes the floor, or
        // interrupts. A press of the gear opens settings instead. A layered
        // window hit-tests by alpha, so neither arrives for a click on the
        // clear corners -- that one goes to whatever is behind, which is what
        // you want from something always on top.
        WM_LBUTTONDOWN => {
            // Where in the window, from the packed coordinates of the message.
            let x = (lparam & 0xffff) as i16 as i32;
            let y = ((lparam >> 16) & 0xffff) as i16 as i32;
            // Decided while borrowed, done after. Opening a window dispatches
            // messages to this same procedure before `CreateWindowExW` returns,
            // and a borrow still held when that happens is a panic -- which is
            // exactly how the first version of this failed.
            let press = ORB.with(|cell| {
                let orb = cell.borrow();
                let orb = orb.as_ref()?;
                // Gated on the pointer being over the orb, which is what
                // decides the gear exists -- not on the fade being far enough
                // along, which would make a click landing inside the first
                // 80 ms take the floor instead. Measured: a fast synthetic
                // click on the gear did exactly that.
                if orb.hovered && on_gear(x, y, orb.canvas.width as f32, orb.scale) {
                    orb.ui.served().map(Press::Settings)
                } else {
                    Some(Press::Talk(orb.ui.clone()))
                }
            });
            match press {
                Some(Press::Talk(ui)) => ui.press_talk(),
                Some(Press::Settings(port)) => open_settings(port),
                None => tracing::error!("settings needs the screen it is served from"),
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: called on the window's own thread, as required.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: forwarding the message exactly as it arrived.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The settings window: an ordinary window with a webview in it.
///
/// A webview, having just spent [0013](../docs/decisions/0013-the-orb-is-an-overlay-on-the-same-stream.md)
/// proving one cannot be transparent. Nothing about a settings window wants to
/// be transparent, and the alternative is drawing text fields and checkboxes
/// with a rasteriser, which is building a GUI toolkit to avoid a dependency.
/// The orb stays drawn because the orb needs alpha; this does not.
///
/// It brings no windowing crate with it. `WebViewBuilder::build` takes anything
/// implementing `HasWindowHandle`, and this thread already owns a window class
/// and a message loop -- so the settings window is another `CreateWindowExW` on
/// the same loop, and [`Handle`] is the fifteen lines that let wry sit in it.
struct Settings {
    hwnd: windows_sys::Win32::Foundation::HWND,
    /// Dropped when the window closes, which is what tears the webview down.
    /// Never read; held.
    _webview: wry::WebView,
}

/// A window handle wry will accept, so no windowing crate is needed for one
/// window.
struct Handle(windows_sys::Win32::Foundation::HWND);

impl raw_window_handle::HasWindowHandle for Handle {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let mut handle = raw_window_handle::Win32WindowHandle::new(
            std::num::NonZeroIsize::new(self.0 as isize)
                .ok_or(raw_window_handle::HandleError::Unavailable)?,
        );
        // SAFETY: the module instance for a window this process created.
        handle.hinstance = std::num::NonZeroIsize::new(unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as isize
        });
        let raw = raw_window_handle::RawWindowHandle::Win32(handle);
        // SAFETY: the HWND outlives this borrow -- `Settings` owns the window
        // and the webview together, and destroys neither before the other.
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

/// The settings window's class, separate from the orb's because it wants the
/// ordinary window procedure and a frame.
const SETTINGS_CLASS: &[u16] = &[
    b'I' as u16, b'R' as u16, b'A' as u16, b'S' as u16, b'e' as u16, b't' as u16, 0,
];
const SETTINGS_SIZE: (i32, i32) = (1000, 720);

/// Opens the settings window, or brings it forward if it is already open.
///
/// Every failure here is logged and swallowed, on the same terms as the rest of
/// this module: no WebView2 runtime, no screen to point it at, a window that
/// will not open. None of them may stop IRA answering questions, and none of
/// them may take the orb down with them.
fn open_settings(port: u16) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindow, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    // The handle is copied out and the borrow dropped before anything is done
    // with it: `SetForegroundWindow` dispatches messages, and so does dropping
    // a `WebView`.
    let open = ORB.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|orb| orb.settings.as_ref().map(|s| s.hwnd))
    });

    if let Some(hwnd) = open {
        // SAFETY: a handle this module created; `IsWindow` tolerates a stale one.
        unsafe {
            if IsWindow(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
                return;
            }
        }
        // Closed behind our back. Take the stale one out and let it drop here,
        // outside the borrow, before building its replacement.
        let stale = ORB.with(|cell| cell.borrow_mut().as_mut().and_then(|o| o.settings.take()));
        drop(stale);
    }

    // The orb's own scale: the settings window opens on the same thread, so it
    // is per-monitor aware too, and its size is in real pixels.
    let scale = ORB.with(|cell| cell.borrow().as_ref().map(|o| o.scale)).unwrap_or(1.0);
    match build_settings(port, scale) {
        // Stored after it is built, so nothing is borrowed while the window
        // being created is sending messages back here.
        Ok(settings) => ORB.with(|cell| {
            if let Some(orb) = cell.borrow_mut().as_mut() {
                orb.settings = Some(settings);
            }
        }),
        Err(e) => tracing::error!("settings window would not open: {e}"),
    }
}

/// What a click on the orb turned out to mean.
enum Press {
    Talk(crate::ui::Ui),
    Settings(u16),
}

fn build_settings(port: u16, scale: f32) -> anyhow::Result<Settings> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, LoadCursorW, RegisterClassW, SetForegroundWindow,
        ShowWindow, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW, SW_SHOW, WNDCLASSW,
        WS_OVERLAPPEDWINDOW,
    };

    // SAFETY: a class registered with a static name and the default window
    // procedure, then a window created from it. The handle is checked.
    let hwnd: HWND = unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(settings_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance as _,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: SETTINGS_CLASS.as_ptr(),
        };
        RegisterClassW(&class);

        let title: Vec<u16> = "IRA — settings"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = CreateWindowExW(
            0,
            SETTINGS_CLASS.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (SETTINGS_SIZE.0 as f32 * scale) as i32,
            (SETTINGS_SIZE.1 as f32 * scale) as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance as _,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return Err(anyhow::anyhow!("CreateWindowEx failed"));
        }
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        hwnd
    };

    // WebView2 keeps a browser profile -- cache, cookies, local storage, a
    // crash handler -- and left to itself it puts it in a folder called
    // `ira.exe.WebView2` beside the executable. For an installed IRA that is
    // the install directory: a few megabytes of browser state written where the
    // program lives, outside the one directory the README promises holds
    // everything, and still there after an uninstall. Found by installing her.
    //
    // ponytail: leaked, because the builder wants a `&mut` that outlives the
    // webview and this is a window someone opens now and then. If settings ever
    // opens in a loop, make it a process-wide singleton.
    let context: &'static mut wry::WebContext = Box::leak(Box::new(wry::WebContext::new(Some(
        crate::paths::in_data("webview"),
    ))));

    // Same origin as the page, so the settings page's saves pass the
    // cross-site guard for the same reason the talk button's presses do.
    let window = hwnd as isize;
    let webview = wry::WebViewBuilder::new_with_web_context(context)
        .with_url(format!("http://127.0.0.1:{port}/settings"))
        // A right-click menu offering "View source" on a settings form is a way
        // to break it, not a feature.
        .with_devtools(false)
        // The page draws the title bar, so its buttons and its drag strip ask
        // the window to do what a title bar would. Nothing else is accepted.
        .with_ipc_handler(move |req| window_command(window as _, req.body()))
        .build(&Handle(hwnd))?;

    Ok(Settings { hwnd, _webview: webview })
}

/// What the settings page's own title bar asked for: minimise, close, a drag
/// from the strip, or a resize from its top edge.
///
/// Posted, not sent, and never done in place: this runs inside a WebView2
/// callback, and a move loop entered here -- or the webview torn down by a
/// close -- would happen under the webview's own feet. Dragging and resizing
/// are the window's own `WM_NCLBUTTONDOWN` handling, so snapping, the move
/// cursor and the resize outline are all Windows'.
fn window_command(hwnd: windows_sys::Win32::Foundation::HWND, what: &str) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, PostMessageW, ShowWindow, HTCAPTION, HTTOP, HTTOPLEFT, HTTOPRIGHT, SW_MINIMIZE,
        WM_CLOSE, WM_NCLBUTTONDOWN,
    };
    let hit = match what {
        "min" => {
            // SAFETY: the window this webview lives in.
            unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
            return;
        }
        "close" => {
            // SAFETY: as above; the close is handled after this returns.
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
            return;
        }
        "drag" => HTCAPTION,
        "n" => HTTOP,
        "nw" => HTTOPLEFT,
        "ne" => HTTOPRIGHT,
        _ => return,
    };
    // SAFETY: a POINT that outlives the call, and a window this module owns.
    // The webview holds the mouse after a press; the window cannot take a
    // drag until it lets go.
    unsafe {
        let mut at = std::mem::zeroed();
        GetCursorPos(&mut at);
        ReleaseCapture();
        let packed = ((at.y as u16 as isize) << 16) | at.x as u16 as isize;
        PostMessageW(hwnd, WM_NCLBUTTONDOWN, hit as usize, packed);
    }
}

/// Closing the settings window must not close IRA.
///
/// The orb's own procedure posts a quit message on `WM_DESTROY`, because the
/// orb going away means the process is going away. This window is the opposite:
/// it is opened and closed as often as you like, and the default handling --
/// destroy the window, leave the message loop alone -- is exactly right.
unsafe extern "system" fn settings_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, GetClientRect, IsZoomed, SetWindowPos, NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER,
        SM_CYFRAME, SWP_NOACTIVATE, SWP_NOZORDER, WM_DESTROY, WM_DPICHANGED, WM_NCCALCSIZE, WM_SIZE,
    };
    match msg {
        // No title bar: the page draws its own. Windows works out the frame as
        // usual and the top of it is given back to the page, so the sides and
        // bottom keep their invisible resize borders, and the window keeps its
        // shadow, rounded corners and snapping -- it is still a framed window,
        // only without a caption. The page's own strip does the top edge.
        WM_NCCALCSIZE if wparam != 0 => {
            // SAFETY: with wparam set, lparam is an NCCALCSIZE_PARAMS Windows
            // owns for the duration of the call.
            unsafe {
                let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                DefWindowProcW(hwnd, msg, wparam, lparam);
                params.rgrc[0].top = top;
                // Maximised, the frame hangs off the screen's edge; without it
                // the top of the page would too.
                if IsZoomed(hwnd) != 0 {
                    let dpi = GetDpiForWindow(hwnd);
                    params.rgrc[0].top +=
                        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                }
            }
            0
        }
        // Dragged to a monitor with another scale, or the scale changed: take
        // the size Windows suggests, which keeps the window the same physical
        // size. The webview follows through WM_SIZE below.
        WM_DPICHANGED => {
            // SAFETY: for this message `lparam` is a RECT Windows owns for the
            // duration of the call.
            unsafe {
                let r = &*(lparam as *const windows_sys::Win32::Foundation::RECT);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            0
        }
        // The webview is a child window and does not resize itself.
        //
        // Deliberately reads nothing shared: this arrives during
        // `CreateWindowExW`, before the window has been stored anywhere, and
        // everything it needs is the window it was sent to.
        WM_SIZE => {
            // SAFETY: a valid HWND and a RECT that outlives the call.
            unsafe {
                let mut rect = std::mem::zeroed();
                if GetClientRect(hwnd, &mut rect) != 0 {
                    resize_webview(hwnd, rect.right, rect.bottom);
                }
            }
            0
        }
        // Forget it, so the next press of the gear builds a new one rather than
        // trying to raise a window that is gone.
        WM_DESTROY => {
            // Taken out under the borrow and dropped after it: dropping a
            // `WebView` tears down a window, which dispatches messages.
            let gone = ORB.with(|cell| {
                let mut orb = cell.try_borrow_mut().ok()?;
                let orb = orb.as_mut()?;
                orb.settings
                    .as_ref()
                    .is_some_and(|s| s.hwnd == hwnd)
                    .then(|| orb.settings.take())
                    .flatten()
            });
            drop(gone);
            0
        }
        // SAFETY: forwarding the message exactly as it arrived.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Stretches the webview's child window over its parent's client area.
///
/// wry sets the bounds itself when it is built as a root webview, but nothing
/// tells it the window has been resized since.
fn resize_webview(parent: windows_sys::Win32::Foundation::HWND, w: i32, h: i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindow, MoveWindow, GW_CHILD};
    // SAFETY: a valid parent handle; `GetWindow` returns null when it has no
    // child, and `MoveWindow` is not called on null.
    unsafe {
        let child = GetWindow(parent, GW_CHILD);
        if !child.is_null() {
            MoveWindow(child, 0, 0, w, h, 1);
        }
    }
}

/// Puts the orb in the bottom-left of the work area of the monitor it is on.
fn settle(hwnd: windows_sys::Win32::Foundation::HWND, side: u32, scale: f32) {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    // SAFETY: a valid HWND.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    settle_on(hwnd, monitor, side, scale);
}

/// Puts the orb in the bottom-left of the work area of `monitor`.
///
/// The work area, not the monitor: the full panel puts the bottom of the orb
/// behind the taskbar. Measured, not guessed -- an earlier version used the
/// monitor size and landed 33 px under it.
///
/// Onto a monitor with another scale, this is placed at the old size and
/// Windows answers with `WM_DPICHANGED`, which rebuilds the orb at the new one
/// and settles it again.
fn settle_on(
    hwnd: windows_sys::Win32::Foundation::HWND,
    monitor: windows_sys::Win32::Graphics::Gdi::HMONITOR,
    side: u32,
    scale: f32,
) {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    // SAFETY: a valid HWND and monitor. `cbSize` is set as the API requires
    // and the struct is read only after a non-zero return.
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        let margin = (MARGIN as f32 * scale) as i32;
        // Stored first: moving to a monitor with another scale settles again
        // from inside `SetWindowPos`, and that one must see where it is going.
        ON.store(monitor as isize, Ordering::Relaxed);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            info.rcWork.left + margin,
            info.rcWork.bottom - side as i32 - margin,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// A device-independent bitmap, and the window it is pushed to.
///
/// Made once and reused: the orb repaints thirty times a second, and allocating
/// a bitmap for each of those would be the most expensive thing in the process.
///
/// Shared with the corner widgets in `hud.rs`, which are the same kind of
/// window: draw into `pixmap`, then [`Canvas::present`].
pub(crate) struct Canvas {
    hwnd: windows_sys::Win32::Foundation::HWND,
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    /// The DIB's own pixels, which `CreateDIBSection` hands back to write into.
    bits: *mut u8,
    pub(crate) pixmap: tiny_skia::Pixmap,
    width: u32,
    height: u32,
    /// So a failing paint is reported once rather than thirty times a second.
    complained: bool,
    /// How opaque the whole window is, 0 to 255, on top of each pixel's own
    /// alpha: what fades the orb out of one monitor and into another.
    pub(crate) alpha: u8,
}

impl Canvas {
    pub(crate) fn new(
        hwnd: windows_sys::Win32::Foundation::HWND,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        use windows_sys::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
            BI_RGB, DIB_RGB_COLORS,
        };

        let pixmap = tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| anyhow::anyhow!("orb bitmap {width}x{height} would not allocate"))?;

        // SAFETY: a zeroed BITMAPINFO filled in as the API requires. `bits` is
        // checked before use, and the DIB owns that memory for as long as the
        // bitmap does -- which is as long as this struct.
        let (dc, bitmap, bits) = unsafe {
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative: top-down, so row 0 is the top and the rows copy
                // straight across from the rasteriser.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };

            let dc = CreateCompatibleDC(std::ptr::null_mut());
            if dc.is_null() {
                return Err(anyhow::anyhow!("orb could not create a device context"));
            }
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap =
                CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                return Err(anyhow::anyhow!("orb could not create its bitmap"));
            }
            SelectObject(dc, bitmap as _);
            (dc, bitmap, bits as *mut u8)
        };

        Ok(Self {
            hwnd,
            dc,
            bitmap,
            bits,
            pixmap,
            width,
            height,
            complained: false,
            alpha: 255,
        })
    }

    /// Draws one frame and pushes it to the window.
    fn draw(&mut self, frame: &Frame, scale: f32, reveal: f32) {
        paint(&mut self.pixmap, frame, scale, reveal);
        self.present();
    }

    /// Pushes whatever is in `pixmap` to the window.
    pub(crate) fn present(&mut self) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION};
        use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

        // tiny-skia is premultiplied RGBA; a DIB is premultiplied BGRA. The same
        // bytes, two of them the other way round.
        let src = self.pixmap.data();
        // SAFETY: `bits` points at width*height*4 bytes owned by the DIB, which
        // is exactly the size of the pixmap of the same dimensions.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, src.len()) };
        let (dst, _) = dst.as_chunks_mut::<4>();
        let (src, _) = src.as_chunks::<4>();
        for (d, s) in dst.iter_mut().zip(src) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }

        let size = windows_sys::Win32::Foundation::SIZE {
            cx: self.width as i32,
            cy: self.height as i32,
        };
        let origin = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: self.alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: every pointer is to a local that outlives the call, and the DC
        // holds the bitmap just filled. A null destination DC and position mean
        // "the screen, where the window already is".
        let ok = unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
                self.dc,
                &origin,
                0,
                &blend,
                ULW_ALPHA,
            )
        };
        // Thirty times a second, so it is said once. A failure here is an orb
        // that is not there at all, which is not a thing to leave silent.
        if ok == 0 && !self.complained {
            self.complained = true;
            // SAFETY: reads this thread's last error, right after the call.
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            tracing::error!("orb could not paint: UpdateLayeredWindow failed, error {err}");
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject};
        // SAFETY: both handles were created here and are not used again.
        unsafe {
            DeleteObject(self.bitmap as _);
            DeleteDC(self.dc);
        }
    }
}

/// Draws the orb: the website's, a dark core with a lit rim, inside a dial.
///
/// Drawn to match the landing page's WebGL orb -- the same state table, the
/// same cyan, the same dial -- with a 2D rasteriser, because a window that is
/// transparent cannot host the webview that would run it
/// ([0013](../docs/decisions/0013-the-orb-is-an-overlay-on-the-same-stream.md)).
/// From the back:
///
/// 1. a dark lens, so it reads the same over a white document as over a dark
///    wallpaper, fading to nothing well inside the window;
/// 2. the dial: ticks, a ring of arc segments, and the arcs that say what she
///    is doing -- working, or how long she has been talking;
/// 3. the orb: a noisy outline filled dark with its rim lit, a shell of dust
///    and two tilted rings passing behind and in front of it;
/// 4. bloom, and the shock ring fired when she wakes or is cut off.
///
/// Every size is in logical pixels of the 160 px window, times `scale`.
fn paint(pixmap: &mut tiny_skia::Pixmap, f: &Frame, scale: f32, reveal: f32) {
    use std::f32::consts::{PI, TAU};
    use tiny_skia::{
        BlendMode, Color, FillRule, FilterQuality, GradientStop, Paint, PathBuilder, PixmapPaint,
        Point, RadialGradient, Shader, SpreadMode, Transform,
    };

    pixmap.fill(Color::TRANSPARENT);

    let (w, h) = (pixmap.width(), pixmap.height());
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let u = scale;
    let lit = lit(f.accent, f.glow);
    // How strongly the dial shows: dimmer when she is off, never brighter than
    // its own colours when she is lit up.
    let dial = 0.6 + 0.4 * f.glow.min(1.0);

    let fill = |pixmap: &mut tiny_skia::Pixmap, path: &tiny_skia::Path, shader: Shader| {
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    };
    let radial = |r: f32, stops: Vec<GradientStop>| {
        RadialGradient::new(
            Point::from_xy(cx, cy),
            Point::from_xy(cx, cy),
            r,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        )
    };

    // --- the lens -------------------------------------------------------
    // Gone by 79 px from the centre, and the corners are 113 away: that is the
    // window's transparency, and what the corner test checks.
    let lens = 79.0 * u;
    if let (Some(shader), Some(path)) = (
        radial(
            lens,
            vec![
                // A short fall-off at the very edge: a long one is a grey
                // smudge on a white page, and buries the outer ring in it.
                GradientStop::new(0.0, rgba(VOID, 0.92)),
                GradientStop::new(0.965, rgba(VOID, 0.88)),
                GradientStop::new(1.0, rgba(VOID, 0.0)),
            ],
        ),
        PathBuilder::from_circle(cx, cy, lens),
    ) {
        fill(pixmap, &path, shader);
    }

    // The orb's light on the lens around it. Never past the lens: out there it
    // has nothing dark under it and shows as a cyan fringe round the window.
    let r = 30.0 * u * f.size;
    let light = (r * 2.1).min(lens * 0.95);
    if let (Some(shader), Some(path)) = (
        radial(
            light,
            vec![
                GradientStop::new(0.0, rgba(f.accent, 0.20 * f.glow)),
                GradientStop::new(1.0, rgba(f.accent, 0.0)),
            ],
        ),
        PathBuilder::from_circle(cx, cy, light),
    ) {
        fill(pixmap, &path, shader);
    }

    // --- the dial -------------------------------------------------------
    // Sixty ticks with every fifth long, turning once every 160 s.
    let turn = f.t / 160.0 * TAU;
    let mut minor = PathBuilder::new();
    let mut major = PathBuilder::new();
    for i in 0..60 {
        let a = turn + i as f32 / 60.0 * TAU;
        let (pb, inner) = if i % 5 == 0 {
            (&mut major, 65.5)
        } else {
            (&mut minor, 68.0)
        };
        pb.move_to(cx + inner * u * a.cos(), cy + inner * u * a.sin());
        pb.line_to(cx + 71.0 * u * a.cos(), cy + 71.0 * u * a.sin());
    }
    if let Some(path) = minor.finish() {
        stroke(pixmap, &path, f.accent, 0.30 * dial, 0.7 * u);
    }
    if let Some(path) = major.finish() {
        stroke(pixmap, &path, f.accent, 0.65 * dial, 1.0 * u);
    }

    // The outer ring of segments, turning the other way, once every 90 s.
    let back = -f.t / 90.0;
    for (from, len) in [
        (0.0, 0.22),
        (0.26, 0.01),
        (0.31, 0.14),
        (0.54, 0.26),
        (0.86, 0.01),
        (0.91, 0.05),
    ] {
        if let Some(path) = arc(cx, cy, 76.0 * u, back + from, len) {
            stroke(pixmap, &path, f.accent, 0.38 * dial, 0.7 * u);
        }
    }

    // The track the working and speaking arcs run on.
    let track = 61.0 * u;
    if let Some(path) = PathBuilder::from_circle(cx, cy, track) {
        stroke(pixmap, &path, f.accent, 0.16 * dial, 1.0 * u);
    }

    // How long she has been talking, from twelve o'clock, clockwise. Amber if
    // she was cut off, stopped where she stopped.
    if f.said_alpha > 0.01 && f.said > 0.002 {
        if let Some(path) = arc(cx, cy, track, -0.25, f.said) {
            let colour = if f.cut { AMBER } else { lit };
            stroke(pixmap, &path, colour, f.said_alpha, 1.8 * u);
        }
    }

    // Working: a short arc chasing round, longer while she thinks.
    let busy = (f.busy / 0.07).min(1.0);
    if busy > 0.01 {
        if let Some(path) = arc(cx, cy, track, f.t / 1.6, f.busy) {
            stroke(pixmap, &path, ICE, 0.9 * busy, 1.8 * u);
        }
    }

    // --- the orb --------------------------------------------------------
    // The outline: heaving noise, plus ripples running down it as she talks.
    let edge = |a: f32| {
        let ripple = f.wave * (a.sin() * 14.0 - f.t * 9.0).sin();
        r * (1.0 + f.amp * heave(a, f.phase) + ripple)
    };

    // Two rings, tilted almost edge-on and swaying a little. The back half of
    // each is drawn before the orb and the front half after, so they pass
    // round it rather than across it.
    let rings = [(1.56f32, 0.25f32, -0.30f32), (1.86, 0.30, 0.40)];
    let ring = |pixmap: &mut tiny_skia::Pixmap, front: bool| {
        for (i, &(size, flat, tilt)) in rings.iter().enumerate() {
            let tilt = tilt + 0.08 * (f.t * 0.3 + i as f32).sin();
            let (rx, ry) = (r * size, r * size * flat);
            let start = if front { 0.0 } else { PI };
            let mut pb = PathBuilder::new();
            for s in 0..=48 {
                let a = start + s as f32 / 48.0 * PI;
                let (x, y) = (rx * a.cos(), ry * a.sin());
                let (x, y) = (
                    cx + x * tilt.cos() - y * tilt.sin(),
                    cy + x * tilt.sin() + y * tilt.cos(),
                );
                if s == 0 {
                    pb.move_to(x, y);
                } else {
                    pb.line_to(x, y);
                }
            }
            if let Some(path) = pb.finish() {
                stroke(pixmap, &path, f.accent, 0.28 * f.glow.min(1.2), 0.6 * u);
            }
        }
    };

    // A shell of dust a third again as wide as the orb, heaving with it and
    // turning slowly, brighter on the near side.
    let dust = |pixmap: &mut tiny_skia::Pixmap, front: bool| {
        const N: usize = 140;
        let (spin, lean) = (f.t * 0.09, 0.3f32);
        for i in 0..N {
            // A Fibonacci sphere: evenly spread, with no clumps at the poles.
            let y = 1.0 - 2.0 * (i as f32 + 0.5) / N as f32;
            let band = (1.0 - y * y).sqrt();
            let around = i as f32 * 2.399_963;
            let (x, z) = (band * around.cos(), band * around.sin());
            let (x, z) = (x * spin.cos() + z * spin.sin(), z * spin.cos() - x * spin.sin());
            let (y, z) = (y * lean.cos() - z * lean.sin(), y * lean.sin() + z * lean.cos());
            if (z >= 0.0) != front {
                continue;
            }
            let reach = r * 1.3 * (1.0 + 1.25 * f.amp * heave(y.atan2(x), f.phase));
            let near = (z + 1.0) / 2.0;
            if let Some(path) = PathBuilder::from_circle(cx + x * reach, cy + y * reach, 0.75 * u) {
                let alpha = f.glow.min(1.0) * (0.2 + 0.6 * near);
                fill(pixmap, &path, Shader::SolidColor(rgba(lit, alpha)));
            }
        }
    };

    ring(pixmap, false);
    dust(pixmap, false);

    let mut outline = PathBuilder::new();
    for s in 0..=96 {
        let a = s as f32 / 96.0 * TAU;
        let (x, y) = (cx + edge(a) * a.cos(), cy + edge(a) * a.sin());
        if s == 0 {
            outline.move_to(x, y);
        } else {
            outline.line_to(x, y);
        }
    }
    outline.close();
    let Some(outline) = outline.finish() else {
        return;
    };

    // Dark through the middle and lit at the rim: the website's fresnel, flat.
    // Measured from the furthest the outline can reach, so where it heaves out
    // it is brightest and where it sinks it dims -- the crests the shader draws.
    let reach = r * (1.0 + f.amp + f.wave);
    if let Some(shader) = radial(
        reach,
        vec![
            GradientStop::new(0.0, rgba(CORE, 1.0)),
            GradientStop::new(0.62, rgba(CORE, 1.0)),
            GradientStop::new(0.86, rgba(mix(CORE, lit, 0.4), 1.0)),
            GradientStop::new(1.0, rgba(lit, 1.0)),
        ],
    ) {
        fill(pixmap, &outline, shader);
    }
    stroke(pixmap, &outline, lit, 0.9, 1.2 * u);

    dust(pixmap, true);
    ring(pixmap, true);

    // --- bloom ----------------------------------------------------------
    // The rim again, thick, at half size, blurred and added on top. Half size
    // for the same reason the pearl orb drew its light that way: a quarter of
    // the blur's cost, and the stretch back up is free softness.
    let (bw, bh) = (w.div_ceil(2), h.div_ceil(2));
    if let Some(mut bloom) = tiny_skia::Pixmap::new(bw, bh) {
        if let Some(rim) = outline.clone().transform(Transform::from_scale(0.5, 0.5)) {
            stroke(&mut bloom, &rim, lit, (0.30 + 0.25 * f.glow).min(1.0), 3.0 * u);
        }
        blur(&mut bloom, (3.0 * u).round().max(1.0) as usize);
        pixmap.draw_pixmap(
            0,
            0,
            bloom.as_ref(),
            &PixmapPaint {
                blend_mode: BlendMode::Plus,
                quality: FilterQuality::Bilinear,
                ..Default::default()
            },
            Transform::from_scale(w as f32 / bw as f32, h as f32 / bh as f32),
            None,
        );
    }

    // The shock ring: out from the rim and gone, ice on waking, amber on a cut.
    if f.shock <= 1.0 {
        let e = 1.0 - (1.0 - f.shock).powi(3);
        if let Some(path) = PathBuilder::from_circle(cx, cy, r * (1.0 + e)) {
            let colour = if f.cut { AMBER } else { ICE };
            stroke(pixmap, &path, colour, (1.0 - e) * 0.7, 1.2 * u);
        }
    }

    // --- the gear --------------------------------------------------------
    // Only while the pointer is over the orb, and eased rather than switched,
    // so it arrives instead of appearing. A lamp with a permanent button on it
    // is a widget; this is meant to be something you stop noticing.
    if reveal > 0.01 {
        gear(pixmap, cx + GEAR_AT.0 * scale, cy + GEAR_AT.1 * scale, scale, reveal);
    }
}

/// How far the surface stands out at angle `a`, roughly -1 to 1.
///
/// The website displaces a sphere with simplex noise. Seen as an outline that
/// is a wobble round a circle, and three sines at unrelated frequencies,
/// drifting against each other, are a wobble round a circle. Every frequency
/// is a whole number so the outline closes.
fn heave(a: f32, phase: f32) -> f32 {
    0.62 * (2.0 * a + phase * 1.7).sin() * (3.0 * a - phase * 1.1).cos()
        + 0.28 * (5.0 * a + phase * 2.3).sin()
        + 0.10 * (9.0 * a - phase * 3.1).sin()
}

/// An arc as a path: `from` and `sweep` in turns, clockwise from three o'clock.
/// Sampled, because tiny-skia has no arc command; a point every 3° is smooth at
/// this size.
fn arc(cx: f32, cy: f32, r: f32, from: f32, sweep: f32) -> Option<tiny_skia::Path> {
    let steps = ((sweep.abs() * 120.0).ceil() as usize).max(2);
    let mut pb = tiny_skia::PathBuilder::new();
    for i in 0..=steps {
        let a = (from + sweep * i as f32 / steps as f32) * std::f32::consts::TAU;
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

fn stroke(
    pixmap: &mut tiny_skia::Pixmap,
    path: &tiny_skia::Path,
    colour: [f32; 3],
    alpha: f32,
    width: f32,
) {
    let paint = tiny_skia::Paint {
        shader: tiny_skia::Shader::SolidColor(rgba(colour, alpha)),
        anti_alias: true,
        ..Default::default()
    };
    let stroke = tiny_skia::Stroke {
        width,
        ..Default::default()
    };
    pixmap.stroke_path(path, &paint, &stroke, tiny_skia::Transform::identity(), None);
}

fn rgba(c: [f32; 3], alpha: f32) -> tiny_skia::Color {
    let ch = |v: f32| v.clamp(0.0, 255.0) as u8;
    tiny_skia::Color::from_rgba8(ch(c[0]), ch(c[1]), ch(c[2]), (alpha.clamp(0.0, 1.0) * 255.0) as u8)
}

/// A colour brightened by a glow, which may go past 1 -- clamped when it is
/// turned into a pixel, so a bright state whitens rather than wraps.
fn lit(c: [f32; 3], glow: f32) -> [f32; 3] {
    c.map(|v| v * glow)
}

fn mix(a: [f32; 3], b: [f32; 3], k: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k]
}

/// The settings gear, at the top-right of the orb.
///
/// Drawn from the same `GEAR_AT` and `GEAR_R` the hit test uses, so the thing
/// you can press is the thing you can see. Dark on light, because it sits on
/// the orb's own glow and a white gear would vanish into it.
fn gear(pixmap: &mut tiny_skia::Pixmap, cx: f32, cy: f32, scale: f32, reveal: f32) {
    use tiny_skia::{Color, FillRule, Paint, PathBuilder, Shader, Transform};

    let r = GEAR_R * scale;
    // Eased, and it grows the last tenth as it arrives, which reads as the
    // button coming to meet you rather than fading up in place.
    let ease = reveal * reveal * (3.0 - 2.0 * reveal);
    let r = r * (0.9 + 0.1 * ease);
    let alpha = |a: f32| (a * ease * 255.0) as u8;

    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };

    // A disc to sit on, so the teeth read against whatever the orb is doing.
    paint.shader = Shader::SolidColor(Color::from_rgba8(255, 255, 255, alpha(0.92)));
    if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    // Eight teeth: a ring of spokes, drawn as one star-shaped path so the fill
    // is a single shape rather than eight overlapping ones.
    paint.shader = Shader::SolidColor(Color::from_rgba8(0x3e, 0x45, 0x4f, alpha(0.95)));
    const TEETH: usize = 8;
    let (inner, outer, half) = (r * 0.46, r * 0.72, 0.34);
    let mut pb = PathBuilder::new();
    for i in 0..TEETH {
        let a = i as f32 / TEETH as f32 * std::f32::consts::TAU;
        let step = std::f32::consts::TAU / TEETH as f32;
        for (radius, at) in [
            (outer, a - step * half),
            (outer, a + step * half),
            (inner, a + step * (0.5 - half)),
            (inner, a + step * (0.5 + half)),
        ] {
            let (x, y) = (cx + radius * at.cos(), cy + radius * at.sin());
            if i == 0 && radius == outer && at < a {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
    }
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    // The hole, punched by drawing the disc colour back over the middle. A
    // gear without one is a cog-shaped blob.
    paint.shader = Shader::SolidColor(Color::from_rgba8(255, 255, 255, alpha(0.92)));
    if let Some(path) = PathBuilder::from_circle(cx, cy, r * 0.27) {
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// A box blur, twice, which is close enough to a gaussian that nobody could
/// tell on a 128 px lamp.
///
/// Prefix sums, so the cost does not grow with the radius: the whole point of
/// this orb is that it is soft, and a radius big enough to look right would be
/// expensive done the obvious way.
fn blur(pixmap: &mut tiny_skia::Pixmap, radius: usize) {
    let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
    if radius == 0 || w == 0 || h == 0 {
        return;
    }
    let mut scratch = vec![0u8; w * h * 4];
    // Room for one leading zero, so a window is one subtraction either way.
    let mut sums = vec![0u32; (w.max(h) + 1) * 4];

    for _ in 0..2 {
        // Across.
        {
            let src = pixmap.data();
            for y in 0..h {
                sums[..4].fill(0);
                for x in 0..w {
                    let (i, o) = ((x + 1) * 4, (y * w + x) * 4);
                    for c in 0..4 {
                        sums[i + c] = sums[i - 4 + c] + src[o + c] as u32;
                    }
                }
                for x in 0..w {
                    let lo = x.saturating_sub(radius);
                    let hi = (x + radius).min(w - 1);
                    let n = (hi - lo + 1) as u32;
                    let o = (y * w + x) * 4;
                    for c in 0..4 {
                        scratch[o + c] = ((sums[(hi + 1) * 4 + c] - sums[lo * 4 + c]) / n) as u8;
                    }
                }
            }
        }
        // Down.
        {
            let dst = pixmap.data_mut();
            for x in 0..w {
                sums[..4].fill(0);
                for y in 0..h {
                    let (i, o) = ((y + 1) * 4, (y * w + x) * 4);
                    for c in 0..4 {
                        sums[i + c] = sums[i - 4 + c] + scratch[o + c] as u32;
                    }
                }
                for y in 0..h {
                    let lo = y.saturating_sub(radius);
                    let hi = (y + radius).min(h - 1);
                    let n = (hi - lo + 1) as u32;
                    let o = (y * w + x) * 4;
                    for c in 0..4 {
                        dst[o + c] = ((sums[(hi + 1) * 4 + c] - sums[lo * 4 + c]) / n) as u8;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state the orb has no `Look` for is a silent failure: the window keeps
    /// whatever it was showing, says nothing, and is simply wrong from then on.
    /// The names come from `main::state_name`, which is the only thing that
    /// emits them.
    #[test]
    fn every_state_the_loop_can_reach_lights_the_orb() {
        for state in [
            crate::State::Idle,
            crate::State::Listening,
            crate::State::Holding,
            crate::State::Confirming,
        ] {
            let name = crate::state_name(&state);
            assert!(
                Look::from_name(name).is_some(),
                "the orb has no look for {name}"
            );
        }
    }

    /// Speaking and dead are decided from the flags rather than the name, and
    /// both have to win over whatever state the loop last reported.
    #[test]
    fn speaking_and_stopping_outrank_the_state() {
        let s = Shared::default();
        s.live.store(true, Ordering::Relaxed);
        s.set(Look::Listening);
        assert_eq!(s.look(), Look::Listening);

        s.speaking.store(true, Ordering::Relaxed);
        assert_eq!(s.look(), Look::Speaking, "speaking must show over listening");

        s.live.store(false, Ordering::Relaxed);
        assert_eq!(s.look(), Look::Dead, "a stopped IRA must not look busy");
    }

    /// The orb has to be transparent where it is not drawn, or it is a square
    /// sitting on top of your work. Painting into a pixmap and reading the
    /// corners back is the whole claim, and it needs no window to check.
    ///
    /// Every state, because the bloom is sized off the state's own energy and a
    /// livelier one must not reach the edge either.
    #[test]
    fn the_corners_are_clear_in_every_state() {
        for look in [
            Look::Idle,
            Look::Listening,
            Look::Thinking,
            Look::Speaking,
            Look::Confirming,
            Look::Dead,
        ] {
            let side = SIZE as u32;
            let mut pixmap = tiny_skia::Pixmap::new(side, side).unwrap();
            // A time that is not zero, so nothing is caught at rest, with the
            // gear out and a shock ring mid-flight: the widest it ever draws.
            let mut frame = Frame::at_rest(look, 3.7);
            frame.shock = 0.3;
            paint(&mut pixmap, &frame, 1.0, 1.0);

            let at = |x: u32, y: u32| pixmap.pixel(x, y).unwrap().alpha();
            let end = side - 1;
            for (x, y) in [(0, 0), (end, 0), (0, end), (end, end)] {
                assert_eq!(at(x, y), 0, "{look:?}: corner {x},{y} must be clear");
            }
            assert!(at(side / 2, side / 2) > 200, "{look:?}: the sphere must be solid");
        }
    }

    /// The arc is the website's argument drawn on a lamp: it follows how much
    /// of the reply she has said, stops where she was cut off and turns amber,
    /// and the next reply starts a clean one.
    #[test]
    fn an_interruption_stops_the_arc_in_amber() {
        let dt = 1.0 / FPS as f32;
        let mut f = Frame::at_rest(Look::Listening, 0.0);
        // A second of her saying 40% of what she has so far.
        for _ in 0..FPS {
            f.step(Look::Speaking, dt, false, 0.4);
        }
        assert!((f.said - 0.4).abs() < 0.01 && f.said_alpha > 0.9 && !f.cut, "{f:?}");

        let said = f.said;
        f.step(Look::Listening, dt, true, 0.4);
        assert!(f.cut, "a barged turn must mark the arc");
        // Even if a late reading arrives, the cut holds the arc where it was.
        f.step(Look::Listening, dt, false, 0.9);
        assert_eq!(f.said, said, "the arc stops where she did");

        // Long enough for it to fade, then she answers something else.
        for _ in 0..FPS * 5 {
            f.step(Look::Listening, dt, false, 0.4);
        }
        f.step(Look::Speaking, dt, false, 0.0);
        assert!(!f.cut && f.said < 0.05, "a new reply starts a new arc: {f:?}");
    }

    /// The rim has to actually be lit. A bloom that washes everything out, an
    /// accent that eases to grey, or a glow stuck at zero would all still paint
    /// a convincing dark disc -- which looks deliberate and is not.
    #[test]
    fn the_sphere_has_colour_in_it() {
        let side = SIZE as u32;
        let mut pixmap = tiny_skia::Pixmap::new(side, side).unwrap();
        paint(&mut pixmap, &Frame::at_rest(Look::Speaking, 3.7), 1.0, 0.0);

        let mut widest = 0i32;
        for y in side / 4..side * 3 / 4 {
            for x in side / 4..side * 3 / 4 {
                let p = pixmap.pixel(x, y).unwrap();
                let (r, g, b) = (p.red() as i32, p.green() as i32, p.blue() as i32);
                widest = widest.max(r.max(g).max(b) - r.min(g).min(b));
            }
        }
        assert!(widest > 18, "the sphere is washed out: widest spread {widest}");
    }

    /// Renders `assets/ira.ico` from the orb itself.
    ///
    /// Ignored, so it is a generator rather than a check: run it when the orb's
    /// look changes and the icon should follow.
    ///
    ///     cargo test render_the_icon -- --ignored
    ///
    /// The installer and the executable both need a Windows icon, and the orb
    /// is already the thing this program looks like -- drawing a second one by
    /// hand would be inventing a logo that has to be kept in step with the one
    /// on screen. Idle rather than a livelier state: an icon is at rest.
    ///
    /// ICO with PNG payloads, which Windows has read since Vista. The container
    /// is a 6-byte header, one 16-byte directory entry per size, and the PNG
    /// files themselves -- less code than a dependency that does it, and the
    /// only writer of this format in the project.
    #[test]
    #[ignore = "writes assets/ira.ico; run it when the orb changes"]
    fn render_the_icon() {
        // Idle, but with the orb larger in its dial than on screen: an icon is
        // read at 16 px, where the orb at its window size is a speck in a dark
        // disc.
        let icon = Frame {
            size: 1.4,
            ..Frame::at_rest(Look::Idle, 0.0)
        };
        // 256 is what Explorer shows at its largest; the smaller ones stop
        // Windows downscaling that one badly in the Start menu and the taskbar.
        let sizes = [16u32, 32, 48, 64, 128, 256];
        let pngs: Vec<Vec<u8>> = sizes
            .iter()
            .map(|&side| {
                let mut pixmap = tiny_skia::Pixmap::new(side, side).unwrap();
                // `paint` sizes everything off the pixmap, so the scale is the
                // ratio to the window the orb was drawn for.
                paint(&mut pixmap, &icon, side as f32 / SIZE as f32, 0.0);
                pixmap.encode_png().expect("encode png")
            })
            .collect();

        let mut ico = Vec::new();
        ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon, not cursor
        ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
        // Images start after the header and the whole directory.
        let mut offset = 6 + 16 * sizes.len() as u32;
        for (&side, png) in sizes.iter().zip(&pngs) {
            // 256 is written as 0: the field is one byte and 256 does not fit.
            ico.push(if side >= 256 { 0 } else { side as u8 });
            ico.push(if side >= 256 { 0 } else { side as u8 });
            ico.push(0); // palette colours, none
            ico.push(0); // reserved
            ico.extend_from_slice(&1u16.to_le_bytes()); // colour planes
            ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
            ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
            ico.extend_from_slice(&offset.to_le_bytes());
            offset += png.len() as u32;
        }
        for png in &pngs {
            ico.extend_from_slice(png);
        }

        let path = std::path::Path::new("assets/ira.ico");
        std::fs::write(path, &ico).expect("write assets/ira.ico");
        eprintln!("wrote {} ({} bytes)", path.display(), ico.len());

        // The same orb as a plain PNG, for the platforms that cannot read an
        // ICO. macOS builds its .icns out of this by resizing, and the largest
        // size it wants is 1024 -- rendered at that size rather than upscaled
        // from the 256 above, since the orb costs nothing to draw again and an
        // upscaled sphere is a blurry one on every Retina display.
        let mut big = tiny_skia::Pixmap::new(1024, 1024).unwrap();
        paint(&mut big, &icon, 1024.0 / SIZE as f32, 0.0);
        let png = big.encode_png().expect("encode png");
        std::fs::write("assets/ira.png", &png).expect("write assets/ira.png");
        eprintln!("wrote assets/ira.png ({} bytes)", png.len());
    }
}
