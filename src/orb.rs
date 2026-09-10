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

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// Logical size of the window, and so of the orb.
const SIZE: f64 = 128.0;
/// Gap from the corner of the work area, so it clears a taskbar.
const MARGIN: f64 = 24.0;
/// Redraws a second. The orb is 128 px of gradient; this is not the expensive
/// part of a process that runs a neural VAD on every 32 ms of audio.
const FPS: u64 = 30;
/// Where the gear sits, in logical pixels from the centre of the window, and
/// how big it is. Up and to the right: the orb lives in the bottom-left corner
/// of the screen, so that is the side with room and the side your pointer
/// arrives from.
const GEAR_AT: (f32, f32) = (37.0, -37.0);
const GEAR_R: f32 = 12.0;

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

    /// The colours moving inside the sphere. A palette rather than one hue,
    /// because an iridescent orb has nothing else to say a state with -- these
    /// and how fast they move is the entire vocabulary.
    ///
    /// Sampled from the reference: cyan #00e6fc, blue #19a2fe, magenta #e559fd,
    /// aqua #71fbf0, pink #fe84e4, lilac #cc9bfd. Cool for waiting, cyan for
    /// listening, the full spread while she talks, warm when she is asking.
    fn palette(self) -> [(u8, u8, u8); 4] {
        match self {
            Look::Idle => [
                (0x51, 0xc8, 0xff),
                (0x78, 0x98, 0xf9),
                (0xcc, 0x9b, 0xfd),
                (0x71, 0xfb, 0xf0),
            ],
            Look::Listening => [
                (0x00, 0xe6, 0xfc),
                (0x71, 0xfb, 0xf0),
                (0x4e, 0xd8, 0xae),
                (0x19, 0xa2, 0xfe),
            ],
            Look::Thinking => [
                (0x78, 0x98, 0xf9),
                (0xcc, 0x9b, 0xfd),
                (0xe5, 0x59, 0xfd),
                (0x19, 0xa2, 0xfe),
            ],
            Look::Speaking => [
                (0x00, 0xe6, 0xfc),
                (0xe5, 0x59, 0xfd),
                (0xfe, 0x84, 0xe4),
                (0x71, 0xfb, 0xf0),
            ],
            Look::Confirming => [
                (0xff, 0x7a, 0x8a),
                (0xfe, 0x84, 0xe4),
                (0xff, 0xb3, 0x6b),
                (0xe5, 0x59, 0xfd),
            ],
            // Not black: a dead orb should look switched off, not broken.
            Look::Dead => [
                (0x9a, 0xa1, 0xac),
                (0x8a, 0x90, 0x9a),
                (0xa6, 0xac, 0xb6),
                (0x92, 0x99, 0xa4),
            ],
        }
    }

    /// How much the light moves, how slowly the colours wander, and how fast
    /// the ribbons flow. One row per state, so the drawing never forks.
    fn motion(self) -> (f32, f32, f32) {
        match self {
            //           energy, seconds per wander, seconds per ribbon
            Look::Idle => (0.45, 26.0, 9.0),
            Look::Listening => (1.0, 13.0, 4.2),
            Look::Thinking => (0.72, 10.0, 5.5),
            Look::Speaking => (1.3, 7.5, 2.4),
            Look::Confirming => (0.35, 18.0, 7.5),
            Look::Dead => (0.0, 90.0, 40.0),
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
        while let Ok(json) = events.recv().await {
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
    started: std::time::Instant,
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
                started: std::time::Instant::now(),
                scale,
                hovered: false,
                reveal: 0.0,
                settings: None,
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

/// Draws one frame, if there is an orb on this thread to draw.
fn frame() {
    ORB.with(|cell| {
        if let Some(orb) = cell.borrow_mut().as_mut() {
            let look = orb.shared.look();
            let t = orb.started.elapsed().as_secs_f32();
            // A frame's worth of fade, toward wherever the pointer is.
            let step = 1.0 / (FPS as f32 * GEAR_FADE);
            let target = if orb.hovered { 1.0 } else { 0.0 };
            orb.reveal = if orb.reveal < target {
                (orb.reveal + step).min(target)
            } else {
                (orb.reveal - step).max(target)
            };
            orb.canvas.draw(look, t, orb.scale, orb.reveal);
        }
    });
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
        DefWindowProcW, PostQuitMessage, WM_DESTROY, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_TIMER,
    };
    match msg {
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
const SETTINGS_SIZE: (i32, i32) = (640, 780);

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

    match build_settings(port) {
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

fn build_settings(port: u16) -> anyhow::Result<Settings> {
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
            SETTINGS_SIZE.0,
            SETTINGS_SIZE.1,
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
    let webview = wry::WebViewBuilder::new_with_web_context(context)
        .with_url(format!("http://127.0.0.1:{port}/settings"))
        // A right-click menu offering "View source" on a settings form is a way
        // to break it, not a feature.
        .with_devtools(false)
        .build(&Handle(hwnd))?;

    Ok(Settings { hwnd, _webview: webview })
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
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, GetClientRect, WM_DESTROY, WM_SIZE,
    };
    match msg {
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
///
/// The work area, not the monitor: the full panel puts the bottom of the orb
/// behind the taskbar. Measured, not guessed -- an earlier version used the
/// monitor size and landed 33 px under it.
fn settle(hwnd: windows_sys::Win32::Foundation::HWND, side: u32, scale: f32) {
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    // SAFETY: a valid HWND. `cbSize` is set as the API requires and the struct
    // is read only after a non-zero return.
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        let margin = (MARGIN as f32 * scale) as i32;
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
struct Canvas {
    hwnd: windows_sys::Win32::Foundation::HWND,
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    /// The DIB's own pixels, which `CreateDIBSection` hands back to write into.
    bits: *mut u8,
    pixmap: tiny_skia::Pixmap,
    width: u32,
    height: u32,
    /// So a failing paint is reported once rather than thirty times a second.
    complained: bool,
}

impl Canvas {
    fn new(
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
        })
    }

    /// Draws one frame and pushes it to the window.
    fn draw(&mut self, look: Look, t: f32, scale: f32, reveal: f32) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION};
        use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

        paint(&mut self.pixmap, look, t, scale, reveal);

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
            SourceConstantAlpha: 255,
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

/// Draws the orb: a pearl sphere with iridescent light moving inside it.
///
/// The look is Siri's, and it is built the way that look is built rather than
/// by drawing anything sphere-shaped. Three layers:
///
/// 1. a soft bloom, which is what makes it read as lit rather than pasted on,
/// 2. a near-white pearl body,
/// 3. an inner layer -- drifting colour blobs and pale ribbons flowing across
///    them -- blurred, then clipped to the sphere so nothing escapes the edge.
///
/// There is no wireframe and there are no bars. State is carried by which
/// colours are in the palette and how fast everything moves, which is the whole
/// vocabulary this kind of orb has.
fn paint(pixmap: &mut tiny_skia::Pixmap, look: Look, t: f32, scale: f32, reveal: f32) {
    use tiny_skia::{
        Color, FillRule, FilterQuality, GradientStop, Mask, Paint, PathBuilder, PixmapPaint, Point,
        RadialGradient, Shader, SpreadMode, Stroke, Transform,
    };

    pixmap.fill(Color::TRANSPARENT);

    let (w, h) = (pixmap.width(), pixmap.height());
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let (amp, drift, flow) = look.motion();
    let palette = look.palette();

    // The sphere breathes with whatever energy the state has. Barely: this is
    // the difference between alive and animated.
    let r = 44.0 * scale * (1.0 + 0.018 * amp * (t * 0.9).sin());

    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };

    // --- the bloom -------------------------------------------------------
    // Fades to fully transparent well inside the window, which is what keeps
    // the corners clear and the window invisible.
    let bloom = r * 1.34;
    let (br, bg, bb) = palette[0];
    if let Some(shader) = RadialGradient::new(
        Point::from_xy(cx, cy),
        Point::from_xy(cx, cy),
        bloom,
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 46)),
            GradientStop::new(0.70, Color::from_rgba8(br, bg, bb, (16.0 + 20.0 * amp) as u8)),
            GradientStop::new(1.0, Color::from_rgba8(br, bg, bb, 0)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        paint.shader = shader;
        if let Some(path) = PathBuilder::from_circle(cx, cy, bloom) {
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    // --- the pearl body --------------------------------------------------
    // Near-white, brightest up and left, with the edge falling away rather than
    // stopping. A hard rim would make it a disc.
    if let Some(shader) = RadialGradient::new(
        Point::from_xy(cx - r * 0.22, cy - r * 0.28),
        Point::from_xy(cx - r * 0.22, cy - r * 0.28),
        r * 1.32,
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 250)),
            GradientStop::new(0.62, Color::from_rgba8(250, 252, 255, 240)),
            GradientStop::new(0.90, Color::from_rgba8(240, 246, 255, 205)),
            GradientStop::new(1.0, Color::from_rgba8(232, 240, 252, 120)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        paint.shader = shader;
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    // --- the light inside ------------------------------------------------
    // Drawn at half size and scaled back up: the blur costs a quarter as much,
    // and the bilinear stretch is free softness on top of it. Nothing here is
    // meant to have an edge.
    let (iw, ih) = (w.div_ceil(2), h.div_ceil(2));
    let Some(mut inner) = tiny_skia::Pixmap::new(iw, ih) else {
        return;
    };
    let s = 0.5;
    let (icx, icy, ir) = (cx * s, cy * s, r * s);

    let mut ink = Paint {
        anti_alias: true,
        ..Default::default()
    };

    // Colour blobs, each wandering its own slow circle. Four is enough for the
    // shifting-oil-slick effect and few enough that they never mush to grey.
    for (i, &(cr, cg, cb)) in palette.iter().enumerate() {
        let phase = i as f32 * std::f32::consts::TAU / palette.len() as f32;
        let wander = t / drift * std::f32::consts::TAU;
        let reach = ir * (0.34 + 0.18 * (wander * 0.7 + phase * 1.7).sin());
        let bx = icx + reach * (wander + phase).cos();
        let by = icy + reach * 0.72 * (wander * 1.3 + phase).sin();
        let blob = ir * (0.92 + 0.12 * (wander * 1.1 + phase).cos());
        let strength = (206.0 + 40.0 * amp).min(255.0) as u8;
        if let Some(shader) = RadialGradient::new(
            Point::from_xy(bx, by),
            Point::from_xy(bx, by),
            blob,
            vec![
                GradientStop::new(0.0, Color::from_rgba8(cr, cg, cb, strength)),
                GradientStop::new(0.52, Color::from_rgba8(cr, cg, cb, strength / 2)),
                GradientStop::new(1.0, Color::from_rgba8(cr, cg, cb, 0)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            ink.shader = shader;
            if let Some(path) = PathBuilder::from_circle(bx, by, blob) {
                inner.fill_path(&path, &ink, FillRule::Winding, Transform::identity(), None);
            }
        }
    }

    // The ribbons: pale, almost white, sweeping across the middle and crossing
    // each other. They are what stops the blobs looking like a lava lamp.
    // Three of them, and the point is that they cross. Given the same tilt and
    // the same wavelength they stack into one broad band instead, which reads
    // as a smear rather than as light folding over itself.
    for i in 0..3 {
        let k = i as f32;
        let phase = k * 2.1;
        let sway = t / flow * std::f32::consts::TAU + phase;
        let seat = (k - 1.0) * ir * 0.20;
        let rise = seat + ir * (0.08 + 0.11 * amp) * (sway * 0.5).sin();
        let thick = ir * (0.13 + 0.06 * (sway * 0.8).cos());
        let alpha = 226 - i * 26;
        ink.shader = Shader::SolidColor(Color::from_rgba8(255, 255, 255, alpha as u8));
        if let Some(path) = ribbon(icx, icy + rise, ir, sway, thick, 1.3 + k * 0.55, (k - 1.0) * 0.26)
        {
            inner.fill_path(&path, &ink, FillRule::Winding, Transform::identity(), None);
        }
    }

    // Small, because this layer is half size: a radius here is worth double at
    // full resolution, and the blobs are gradients that arrive soft already.
    // The blur is for the ribbons.
    blur(&mut inner, (2.0 * scale).round().max(1.0) as usize);

    // Clipped to the sphere, so the light stays inside the glass.
    if let Some(mut mask) = Mask::new(w, h) {
        if let Some(path) = PathBuilder::from_circle(cx, cy, r * 0.995) {
            mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
            pixmap.draw_pixmap(
                0,
                0,
                inner.as_ref(),
                &PixmapPaint {
                    quality: FilterQuality::Bilinear,
                    ..Default::default()
                },
                Transform::from_scale(w as f32 / iw as f32, h as f32 / ih as f32),
                Some(&mask),
            );
        }
    }

    // --- the gear --------------------------------------------------------
    // Only while the pointer is over the orb, and eased rather than switched,
    // so it arrives instead of appearing. A lamp with a permanent button on it
    // is a widget; this is meant to be something you stop noticing.
    if reveal > 0.01 {
        gear(pixmap, cx + GEAR_AT.0 * scale, cy + GEAR_AT.1 * scale, scale, reveal);
    }

    // --- the rim ---------------------------------------------------------
    // A bright hairline, brightest where the light is coming from. It is what
    // tells you the thing is a sphere and not a hole.
    if let Some(shader) = RadialGradient::new(
        Point::from_xy(cx - r * 0.35, cy - r * 0.45),
        Point::from_xy(cx - r * 0.35, cy - r * 0.45),
        r * 2.1,
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 235)),
            GradientStop::new(1.0, Color::from_rgba8(255, 255, 255, 90)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        paint.shader = shader;
        let stroke = Stroke {
            width: 1.1 * scale,
            ..Default::default()
        };
        if let Some(path) = PathBuilder::from_circle(cx, cy, r - 0.5 * scale) {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
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

/// One ribbon: a long lens shape crossing the sphere, its centreline waved and
/// its thickness tapering to nothing at both ends.
///
/// Sampled rather than drawn as curves. Twenty-four points is smooth enough at
/// this size, and the blur that follows would hide worse than that anyway.
fn ribbon(
    cx: f32,
    cy: f32,
    r: f32,
    sway: f32,
    thick: f32,
    freq: f32,
    tilt: f32,
) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;

    const STEPS: usize = 24;
    let (x0, x1) = (cx - r * 1.02, cx + r * 1.02);
    let centre = |f: f32| {
        cy + (f * std::f32::consts::PI * freq + sway).sin() * r * 0.16 + tilt * (f - 0.5) * r
    };
    let half = |f: f32| thick * (f * std::f32::consts::PI).sin().max(0.0);

    let mut pb = PathBuilder::new();
    for i in 0..=STEPS {
        let f = i as f32 / STEPS as f32;
        let x = x0 + (x1 - x0) * f;
        let y = centre(f) - half(f);
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    for i in (0..=STEPS).rev() {
        let f = i as f32 / STEPS as f32;
        pb.line_to(x0 + (x1 - x0) * f, centre(f) + half(f));
    }
    pb.close();
    pb.finish()
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
            let mut pixmap = tiny_skia::Pixmap::new(128, 128).unwrap();
            // A time that is not zero, so nothing is caught at rest.
            paint(&mut pixmap, look, 3.7, 1.0, 0.0);

            let at = |x: u32, y: u32| pixmap.pixel(x, y).unwrap().alpha();
            for (x, y) in [(0, 0), (127, 0), (0, 127), (127, 127)] {
                assert_eq!(at(x, y), 0, "{look:?}: corner {x},{y} must be clear");
            }
            assert!(at(64, 64) > 200, "{look:?}: the sphere must be solid");
        }
    }

    /// The light inside has to actually be coloured. A blur bug, a mask that
    /// clips everything, or a palette left grey would all still paint a sphere
    /// -- a white one, which looks deliberate and is not.
    #[test]
    fn the_sphere_has_colour_in_it() {
        let mut pixmap = tiny_skia::Pixmap::new(128, 128).unwrap();
        paint(&mut pixmap, Look::Speaking, 3.7, 1.0, 0.0);

        let mut widest = 0i32;
        for y in 40..88 {
            for x in 40..88 {
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
        // 256 is what Explorer shows at its largest; the smaller ones stop
        // Windows downscaling that one badly in the Start menu and the taskbar.
        let sizes = [16u32, 32, 48, 64, 128, 256];
        let pngs: Vec<Vec<u8>> = sizes
            .iter()
            .map(|&side| {
                let mut pixmap = tiny_skia::Pixmap::new(side, side).unwrap();
                // `paint` sizes everything off the pixmap, so the scale is the
                // ratio to the 128 px window the orb was drawn for.
                paint(&mut pixmap, Look::Idle, 0.0, side as f32 / 128.0, 0.0);
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
        paint(&mut big, Look::Idle, 0.0, 8.0, 0.0);
        let png = big.encode_png().expect("encode png");
        std::fs::write("assets/ira.png", &png).expect("write assets/ira.png");
        eprintln!("wrote assets/ira.png ({} bytes)", png.len());
    }
}
