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
#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn spawn(_ui: &crate::ui::Ui) {
    tracing::debug!("the orb is windows-only");
}

// Everything the window procedure needs. There is one orb per process and it
// lives on the thread that owns the window, so a thread local is the whole of
// it: no pointer to smuggle through `GWLP_USERDATA` and get wrong.
#[cfg(windows)]
thread_local! {
    static ORB: std::cell::RefCell<Option<Orb>> = const { std::cell::RefCell::new(None) };
}

#[cfg(windows)]
struct Orb {
    canvas: Canvas,
    shared: Arc<Shared>,
    ui: crate::ui::Ui,
    started: std::time::Instant,
    scale: f32,
}

/// The window class name, UTF-16 because that is what `RegisterClassW` reads.
#[cfg(windows)]
const CLASS: &[u16] = &[b'I' as u16, b'R' as u16, b'A' as u16, b'O' as u16, b'r' as u16, b'b' as u16, 0];

/// The frame timer's id. Any non-zero value; there is only ever one.
#[cfg(windows)]
const TIMER: usize = 1;

#[cfg(windows)]
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
#[cfg(windows)]
fn frame() {
    ORB.with(|cell| {
        if let Some(orb) = cell.borrow_mut().as_mut() {
            let look = orb.shared.look();
            let t = orb.started.elapsed().as_secs_f32();
            orb.canvas.draw(look, t, orb.scale);
        }
    });
}

#[cfg(windows)]
unsafe extern "system" fn wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, WM_DESTROY, WM_LBUTTONDOWN, WM_TIMER,
    };
    match msg {
        WM_TIMER => {
            frame();
            0
        }
        // A press of the orb is the talk control: it takes the floor, or
        // interrupts. A layered window hit-tests by alpha, so this never arrives
        // for a click on the clear corners -- that one goes to whatever is
        // behind, which is what you want from something always on top.
        WM_LBUTTONDOWN => {
            ORB.with(|cell| {
                if let Some(orb) = cell.borrow().as_ref() {
                    orb.ui.press_talk();
                }
            });
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

/// Puts the orb in the bottom-left of the work area of the monitor it is on.
///
/// The work area, not the monitor: the full panel puts the bottom of the orb
/// behind the taskbar. Measured, not guessed -- an earlier version used the
/// monitor size and landed 33 px under it.
#[cfg(windows)]
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
#[cfg(windows)]
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

#[cfg(windows)]
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
    fn draw(&mut self, look: Look, t: f32, scale: f32) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION};
        use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

        paint(&mut self.pixmap, look, t, scale);

        // tiny-skia is premultiplied RGBA; a DIB is premultiplied BGRA. The same
        // bytes, two of them the other way round.
        let src = self.pixmap.data();
        // SAFETY: `bits` points at width*height*4 bytes owned by the DIB, which
        // is exactly the size of the pixmap of the same dimensions.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, src.len()) };
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
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

#[cfg(windows)]
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
#[cfg(windows)]
fn paint(pixmap: &mut tiny_skia::Pixmap, look: Look, t: f32, scale: f32) {
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

/// One ribbon: a long lens shape crossing the sphere, its centreline waved and
/// its thickness tapering to nothing at both ends.
///
/// Sampled rather than drawn as curves. Twenty-four points is smooth enough at
/// this size, and the blur that follows would hide worse than that anyway.
#[cfg(windows)]
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
#[cfg(windows)]
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

#[cfg(all(test, windows))]
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
            paint(&mut pixmap, look, 3.7, 1.0);

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
        paint(&mut pixmap, Look::Speaking, 3.7, 1.0);

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
}
