//! The corner widgets: what IRA is doing at the top left, and the machine she
//! runs on at the top right.
//!
//! [decisions/0023](../docs/decisions/0023-the-corners-show-what-she-is-doing.md).
//! The orb is a light and says one thing. These say the rest, in words: what
//! she heard and said, how long the last turn took and where the time went,
//! what she last did, what she is running on; and on the other side the time,
//! the machine's load, its power and network, and the next reminders.
//!
//! **The same kind of window as the orb.** Layered, topmost, drawn with
//! tiny-skia and pushed with the orb's own [`Canvas`]. A webview cannot be
//! transparent (0013), and these sit over your work. They are also
//! click-through -- `WS_EX_TRANSPARENT` -- so they never take a click meant for
//! the window underneath. Text is drawn with swash, from the same Archivo file
//! the settings page loads, at the website's widths.
//!
//! **They read the same broadcast as the orb.** Subscribing is not watching,
//! exactly as there: IRA does not tell the model there is a screen because
//! these are up, so she never claims to have put the detail on one.
//!
//! **Hidden from the settings window.** `IRA_WIDGETS`, read on every frame, so
//! hiding them is immediate and needs no restart.
//!
//! Every failure here is a missing panel and never a broken IRA, on the same
//! terms as `orb.rs`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use crate::orb::Canvas;

/// Logical sizes of the two panels, before the display's scale.
const AI_SIZE: (f32, f32) = (380.0, 316.0);
const SYS_SIZE: (f32, f32) = (300.0, 316.0);
/// From the corner of the work area, the same as the orb's.
const MARGIN: f32 = 24.0;
/// Inside a panel.
const PAD: f32 = 24.0;
/// Redraws a second. Nothing on either panel changes faster than a state does,
/// and a quarter of a second is quicker than anyone reads one.
const FPS: u32 = 4;

// The website's palette, as bytes.
const CYAN: [u8; 3] = [47, 224, 208];
const ICE: [u8; 3] = [159, 243, 234];
const AMBER: [u8; 3] = [232, 180, 85];
const GREY: [u8; 3] = [138, 150, 156];
const PAPER: [u8; 3] = [227, 238, 240];
const DIM: [u8; 3] = [134, 160, 168];
const FAINT: [u8; 3] = [90, 113, 122];
const LINE: [u8; 3] = [21, 35, 42];
const BAD: [u8; 3] = [240, 138, 148];
const VOID: [u8; 3] = [5, 8, 10];

/// A way of setting text: size in logical pixels, Archivo's width and weight,
/// and tracking in ems.
#[derive(Clone, Copy)]
struct Style {
    size: f32,
    wdth: f32,
    wght: f32,
    track: f32,
}

/// Small readouts and labels: expanded and tracked, as on the website.
const READOUT: Style = Style { size: 11.0, wdth: 125.0, wght: 500.0, track: 0.06 };
/// The "IRA" mark.
const MARK: Style = Style { size: 12.0, wdth: 125.0, wght: 600.0, track: 0.18 };
/// Words to read.
const BODY: Style = Style { size: 13.5, wdth: 100.0, wght: 400.0, track: 0.0 };
const SMALL: Style = Style { size: 12.5, wdth: 100.0, wght: 400.0, track: 0.0 };
const CLOCK: Style = Style { size: 44.0, wdth: 125.0, wght: 300.0, track: -0.02 };

// ------------------------------------------------------------------ what --

/// What the left panel shows, folded from the broadcast.
#[derive(Clone, Debug, Default)]
struct Ai {
    /// The loop's state name, as `main::state_name` emits it.
    state: String,
    speaking: bool,
    /// False once the broadcast has closed: IRA is on her way out.
    live: bool,
    heard: String,
    reply: String,
    /// Her yes-or-no question, while she waits for the answer.
    asking: Option<String>,
    turn: Option<Times>,
    action: Option<Action>,
}

/// The last turn's timings: the three stages of `metrics::Turn::event`, which
/// add up to the total, and whether you cut her off.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Times {
    hear: u64,
    think: u64,
    voice: u64,
    total: u64,
    barged: bool,
}

/// The last tool or skill she used. `ok` is `None` while it is still running.
#[derive(Clone, Debug, PartialEq)]
struct Action {
    name: String,
    ok: Option<bool>,
    text: String,
}

impl Ai {
    /// Folds one event into what the panel shows.
    fn apply(&mut self, v: &serde_json::Value) {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        match v["kind"].as_str() {
            Some("state") => self.state = s("name"),
            Some("speaking") => self.speaking = v["on"].as_bool().unwrap_or(false),
            // A new question: the old answer no longer answers anything.
            Some("heard") => {
                self.heard = s("text");
                self.reply.clear();
            }
            Some("reply") => self.reply = s("text"),
            Some("confirm") => self.asking = Some(s("question")),
            Some("answered") => self.asking = None,
            Some("tool") => {
                self.action = Some(Action { name: s("name"), ok: None, text: String::new() });
            }
            Some("result") => {
                self.action = Some(Action {
                    name: s("name"),
                    ok: v["ok"].as_bool(),
                    text: s("text").lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().into(),
                });
            }
            Some("failed") => {
                self.action = Some(Action { name: "Failed".into(), ok: Some(false), text: s("what") });
            }
            Some("turn") => {
                self.turn = Some(Times {
                    hear: n("stt_ms"),
                    think: n("ttft_ms"),
                    voice: n("tts_ms"),
                    total: n("total_ms"),
                    barged: v["barged"].as_bool().unwrap_or(false),
                });
            }
            _ => {}
        }
    }

    /// What to call the state, and in what colour: amber when she is asking,
    /// grey when she has stopped, as on the orb.
    fn label(&self) -> (&'static str, [u8; 3]) {
        if !self.live {
            return ("Stopped", GREY);
        }
        if self.speaking {
            return ("Speaking", CYAN);
        }
        match self.state.as_str() {
            "listening" => ("Listening", CYAN),
            "holding" => ("Thinking", CYAN),
            "confirming" => ("Asking you", AMBER),
            _ => ("Idle", DIM),
        }
    }
}

/// What she is running on: read once a second rather than per frame, because
/// settings can change under her but not four times a second.
#[derive(Clone, Debug, Default)]
struct Info {
    model: String,
    hearing: &'static str,
    voice: String,
    servers: usize,
}

impl Info {
    fn read(ui: &crate::ui::Ui) -> Self {
        let voice = if crate::settings::get("IRA_TTS_ENGINE").as_deref() == Some("piper") {
            "Piper".to_string()
        } else {
            let id = crate::settings::get("IRA_KOKORO_VOICE")
                .unwrap_or_else(|| crate::kokoro::VOICES[0].0.to_string());
            let name = crate::kokoro::VOICES
                .iter()
                .find(|(v, _)| *v == id)
                .and_then(|(_, label)| label.split(" ·").next())
                .unwrap_or(&id)
                .to_string();
            format!("Kokoro, {name}")
        };
        // Live means connected and offering tools, the settings window's
        // meaning of it -- not merely configured.
        let servers = ui
            .host()
            .map(|h| h.by_server().into_iter().map(|(s, _)| s).collect::<BTreeSet<_>>().len())
            .unwrap_or(0);
        Self {
            model: crate::settings::get("IRA_LLM_MODEL").unwrap_or_else(|| crate::llm::MODEL.into()),
            hearing: if crate::stt::local() { "Local" } else { "Groq" },
            voice,
            servers,
        }
    }
}

/// The right panel's readings.
#[derive(Default)]
struct Machine {
    /// Idle, kernel and user time at the last reading, for the next delta.
    cpu_at: Option<(u64, u64, u64)>,
    cpu: f32,
    /// Eased toward `cpu`, so the bar glides rather than ticks.
    cpu_shown: f32,
    mem: f32,
    mem_shown: f32,
    battery: Option<(u8, bool)>,
    online: bool,
    reminders: Vec<(i64, String)>,
    ticks: u32,
}

impl Machine {
    /// Takes whatever readings are due on this frame.
    fn sample(&mut self) {
        if self.ticks.is_multiple_of(FPS) {
            if let Some(now) = cpu_times() {
                if let Some(busy) = self.cpu_at.and_then(|prev| busy(prev, now)) {
                    self.cpu = busy;
                }
                self.cpu_at = Some(now);
            }
            self.mem = memory_load();
            self.battery = battery();
        }
        // Slower ones: a database read and a network check are cheap, but not
        // four-times-a-second cheap, and neither changes that quickly.
        if self.ticks.is_multiple_of(FPS * 5) {
            self.online = online();
            let now = crate::audit::now_ms();
            self.reminders = crate::db::reminders()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.due >= now - 60_000)
                .take(2)
                .map(|r| (r.due, r.text))
                .collect();
        }
        self.cpu_shown += (self.cpu - self.cpu_shown) * 0.5;
        self.mem_shown += (self.mem - self.mem_shown) * 0.5;
        self.ticks = self.ticks.wrapping_add(1);
    }
}

/// How busy the processors were between two readings, 0 to 1. Kernel time
/// includes idle time, which is why idle comes off the sum of both.
fn busy(prev: (u64, u64, u64), now: (u64, u64, u64)) -> Option<f32> {
    let idle = now.0.saturating_sub(prev.0);
    let total = now.1.saturating_sub(prev.1) + now.2.saturating_sub(prev.2);
    (total > 0).then(|| (1.0 - idle as f32 / total as f32).clamp(0.0, 1.0))
}

fn cpu_times() -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;
    let zero = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut idle, mut kernel, mut user) = (zero(), zero(), zero());
    // SAFETY: three FILETIMEs that outlive the call.
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    let n = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    (ok != 0).then(|| (n(idle), n(kernel), n(user)))
}

fn memory_load() -> f32 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a zeroed MEMORYSTATUSEX with its length set, as the API requires.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 {
            return 0.0;
        }
        m.dwMemoryLoad as f32 / 100.0
    }
}

/// Charge and whether it is charging, or `None` on a machine with no battery
/// -- which is a desktop, and gets no battery line at all.
fn battery() -> Option<(u8, bool)> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // SAFETY: a zeroed struct the call fills in.
    let s = unsafe {
        let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
        if GetSystemPowerStatus(&mut s) == 0 {
            return None;
        }
        s
    };
    // 128 is "no system battery", 255 is "unknown"; neither has a level.
    if s.BatteryFlag & 128 != 0 || s.BatteryFlag == 255 || s.BatteryLifePercent > 100 {
        return None;
    }
    Some((s.BatteryLifePercent, s.BatteryFlag & 8 != 0))
}

fn online() -> bool {
    use windows_sys::Win32::Networking::WinInet::InternetGetConnectedState;
    let mut flags = 0;
    // SAFETY: a flags word that outlives the call; the reserved argument is 0.
    unsafe { InternetGetConnectedState(&mut flags, 0) != 0 }
}

/// "12 min", "2 h 5 min", "3 d": short enough to sit beside a reminder.
fn countdown(ms: i64) -> String {
    if ms < 60_000 {
        return "now".into();
    }
    let mins = (ms + 59_999) / 60_000;
    match mins {
        m if m < 60 => format!("{m} min"),
        m if m < 1440 && m % 60 == 0 => format!("{} h", m / 60),
        m if m < 1440 => format!("{} h {} min", m / 60, m % 60),
        m => format!("{} d", m / 1440),
    }
}

// ------------------------------------------------------------------ text --

/// Archivo, scaled and rendered by swash, cached per glyph.
struct Text {
    font: swash::FontRef<'static>,
    ctx: swash::scale::ScaleContext,
    /// By glyph, size in eighths of a pixel, width and weight. Bounded by the
    /// characters that ever appear in a handful of sizes, so never cleared.
    glyphs: HashMap<(u16, u32, u16, u16), Option<swash::scale::image::Image>>,
}

impl Text {
    fn new() -> Option<Self> {
        Some(Self {
            font: swash::FontRef::from_index(crate::ui::ARCHIVO_FONT, 0)?,
            ctx: swash::scale::ScaleContext::new(),
            glyphs: HashMap::new(),
        })
    }

    fn coords(&self, st: Style) -> Vec<i16> {
        self.font
            .variations()
            .normalized_coords([("wdth", st.wdth), ("wght", st.wght)])
            .collect()
    }

    /// Width of `s` in device pixels at `scale`.
    fn width(&self, s: &str, st: Style, scale: f32) -> f32 {
        let px = st.size * scale;
        let coords = self.coords(st);
        let metrics = self.font.glyph_metrics(&coords).scale(px);
        let map = self.font.charmap();
        let n = s.chars().count() as f32;
        s.chars().map(|c| metrics.advance_width(map.map(c))).sum::<f32>()
            + st.track * px * (n - 1.0).max(0.0)
    }

    /// Draws `s` from `(x, y)`, `y` being the baseline, in device pixels.
    fn draw(
        &mut self,
        pixmap: &mut tiny_skia::Pixmap,
        s: &str,
        (x, y): (f32, f32),
        st: Style,
        scale: f32,
        rgb: [u8; 3],
    ) {
        let font = self.font;
        let px = st.size * scale;
        let coords = self.coords(st);
        let metrics = font.glyph_metrics(&coords).scale(px);
        let map = font.charmap();
        let mut pen = x;
        for c in s.chars() {
            let id = map.map(c);
            let key = (id, (px * 8.0) as u32, st.wdth as u16, st.wght as u16);
            if !self.glyphs.contains_key(&key) {
                let mut scaler = self
                    .ctx
                    .builder(font)
                    .size(px)
                    .hint(false)
                    .variations([("wdth", st.wdth), ("wght", st.wght)])
                    .build();
                let image = swash::scale::Render::new(&[swash::scale::Source::Outline])
                    .format(swash::zeno::Format::Alpha)
                    .render(&mut scaler, id);
                self.glyphs.insert(key, image);
            }
            if let Some(Some(image)) = self.glyphs.get(&key) {
                blit(pixmap, image, pen.round() as i32, y.round() as i32, rgb);
            }
            pen += metrics.advance_width(id) + st.track * px;
        }
    }
}

/// Lays a glyph's coverage over the pixmap in a colour: premultiplied
/// source-over, by hand, because the coverage is a bare alpha mask.
fn blit(pixmap: &mut tiny_skia::Pixmap, image: &swash::scale::image::Image, x: i32, y: i32, rgb: [u8; 3]) {
    let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
    let p = image.placement;
    let data = pixmap.data_mut();
    for row in 0..p.height as i32 {
        let py = y - p.top + row;
        if py < 0 || py >= h {
            continue;
        }
        for col in 0..p.width as i32 {
            let px = x + p.left + col;
            if px < 0 || px >= w {
                continue;
            }
            let a = image.data[(row * p.width as i32 + col) as usize] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            let i = ((py * w + px) * 4) as usize;
            for (c, &v) in rgb.iter().enumerate() {
                data[i + c] = (v as f32 * a + data[i + c] as f32 * (1.0 - a)).round() as u8;
            }
            data[i + 3] = (255.0 * a + data[i + 3] as f32 * (1.0 - a)).round() as u8;
        }
    }
}

/// The longest start of `s` that fits, ending in an ellipsis if anything was
/// cut. `fits` is the measure, so this is testable without a font.
fn fit(s: &str, fits: impl Fn(&str) -> bool) -> String {
    if fits(s) {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    for n in (0..chars.len()).rev() {
        let cut = format!("{}…", chars[..n].iter().collect::<String>().trim_end());
        if fits(&cut) {
            return cut;
        }
    }
    String::new()
}

/// `s` broken into at most `lines` lines that fit, the last cut short with an
/// ellipsis if the text runs on past it.
fn wrap(s: &str, lines: usize, fits: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut words = s.split_whitespace().peekable();
    while let Some(first) = words.next() {
        if out.len() + 1 == lines {
            let rest: Vec<&str> = std::iter::once(first).chain(words).collect();
            out.push(fit(&rest.join(" "), &fits));
            break;
        }
        let mut line = first.to_string();
        while let Some(next) = words.peek() {
            let longer = format!("{line} {next}");
            if !fits(&longer) {
                break;
            }
            line = longer;
            words.next();
        }
        // A single word longer than the line is cut rather than overflowing.
        out.push(fit(&line, &fits));
    }
    out
}

// --------------------------------------------------------------- drawing --

/// One panel being drawn: the pixmap, the text, and the scale every logical
/// coordinate is multiplied by.
struct Pen<'a> {
    p: &'a mut tiny_skia::Pixmap,
    text: &'a mut Text,
    u: f32,
}

impl Pen<'_> {
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3], alpha: f32) {
        let u = self.u;
        // At least a device pixel, so a hairline never rounds away.
        let (w, h) = ((w * u).max(1.0), (h * u).max(1.0));
        if let Some(r) = tiny_skia::Rect::from_xywh(x * u, y * u, w, h) {
            let paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(colour(rgb, alpha)),
                ..Default::default()
            };
            self.p.fill_rect(r, &paint, tiny_skia::Transform::identity(), None);
        }
    }

    fn dot(&mut self, x: f32, y: f32, r: f32, rgb: [u8; 3]) {
        let u = self.u;
        if let Some(path) = tiny_skia::PathBuilder::from_circle(x * u, y * u, r * u) {
            let paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(colour(rgb, 1.0)),
                anti_alias: true,
                ..Default::default()
            };
            self.p
                .fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
        }
    }

    /// Width in logical pixels.
    fn width(&self, s: &str, st: Style) -> f32 {
        self.text.width(s, st, self.u) / self.u
    }

    /// Draws `s` from `x`; returns where it ends.
    fn say(&mut self, s: &str, x: f32, y: f32, st: Style, rgb: [u8; 3]) -> f32 {
        let u = self.u;
        self.text.draw(self.p, s, (x * u, y * u), st, u, rgb);
        x + self.width(s, st)
    }

    /// Draws `s` ending at `right`; returns where it starts.
    fn say_right(&mut self, s: &str, right: f32, y: f32, st: Style, rgb: [u8; 3]) -> f32 {
        let x = right - self.width(s, st);
        self.say(s, x, y, st, rgb);
        x
    }

    /// `s` cut to fit between `x` and `right`.
    fn say_fit(&mut self, s: &str, x: f32, right: f32, y: f32, st: Style, rgb: [u8; 3]) {
        let room = right - x;
        let s = fit(s, |t| self.width(t, st) <= room);
        self.say(&s, x, y, st, rgb);
    }

    /// The panel itself: the orb's dark lens as a rectangle, with the website's
    /// corner brackets.
    fn panel(&mut self, w: f32, h: f32) {
        self.p.fill(tiny_skia::Color::TRANSPARENT);
        // Nearly opaque: whatever is underneath is usually text too, and any
        // more of it showing through fights the words on the panel.
        self.rect(0.0, 0.0, w, h, VOID, 0.94);
        let k = 14.0;
        for (x, y, dx, dy) in [(0.0, 0.0, 1.0, 1.0), (w, 0.0, -1.0, 1.0), (0.0, h, 1.0, -1.0), (w, h, -1.0, -1.0)] {
            let x0 = if dx > 0.0 { x } else { x - k };
            let y0 = if dy > 0.0 { y } else { y - 1.0 };
            self.rect(x0, y0, k, 1.0, CYAN, 0.55);
            let x1 = if dx > 0.0 { x } else { x - 1.0 };
            let y1 = if dy > 0.0 { y } else { y - k };
            self.rect(x1, y1, 1.0, k, CYAN, 0.55);
        }
    }

    /// A thin bar with its track, `frac` of the way along.
    fn bar(&mut self, x: f32, y: f32, w: f32, frac: f32, rgb: [u8; 3]) {
        self.rect(x, y, w, 2.0, LINE, 1.0);
        if frac > 0.0 {
            self.rect(x, y, w * frac.clamp(0.0, 1.0), 2.0, rgb, 1.0);
        }
    }
}

fn colour(rgb: [u8; 3], alpha: f32) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(rgb[0], rgb[1], rgb[2], (alpha.clamp(0.0, 1.0) * 255.0) as u8)
}

/// The left panel: her state, the conversation, the last turn, the last
/// thing she did, and what she runs on.
fn draw_ai(pen: &mut Pen, ai: &Ai, info: &Info) {
    let (w, h) = AI_SIZE;
    let right = w - PAD;
    pen.panel(w, h);

    pen.say("IRA", PAD, 38.0, MARK, PAPER);
    let (label, rgb) = ai.label();
    let at = pen.say_right(label, right, 38.0, READOUT, rgb);
    pen.dot(at - 9.0, 34.0, 3.0, rgb);

    // The conversation.
    let col = 64.0;
    if ai.heard.is_empty() && ai.reply.is_empty() && ai.asking.is_none() {
        pen.say("Say \u{201c}hey Jarvis\u{201d}, or click the orb.", PAD, 84.0, BODY, DIM);
    } else {
        pen.say("You", PAD, 84.0, READOUT, FAINT);
        pen.say_fit(&ai.heard, col, right, 84.0, BODY, PAPER);
        pen.say("IRA", PAD, 108.0, READOUT, FAINT);
        let (said, rgb) = match &ai.asking {
            Some(q) => (q.as_str(), AMBER),
            None => (ai.reply.as_str(), ICE),
        };
        if said.is_empty() {
            pen.say("\u{2026}", col, 108.0, BODY, FAINT);
        }
        let room = right - col;
        let lines = wrap(said, 2, |t| pen.width(t, BODY) <= room);
        for (i, line) in lines.iter().enumerate() {
            pen.say(line, col, 108.0 + 20.0 * i as f32, BODY, rgb);
        }
    }

    // The last turn, the website's timeline in one bar: hearing, thinking,
    // then her voice, in proportion.
    pen.say("Last turn", PAD, 170.0, READOUT, FAINT);
    match ai.turn {
        None => {
            pen.say_right("None yet", right, 170.0, READOUT, FAINT);
            pen.bar(PAD, 182.0, right - PAD, 0.0, CYAN);
        }
        Some(t) => {
            let total = format!("{:.2} s", t.total as f32 / 1000.0);
            let at = pen.say_right(&total, right, 170.0, READOUT, PAPER);
            if t.barged {
                pen.say_right("Cut in", at - 10.0, 170.0, READOUT, AMBER);
            }
            let span = right - PAD;
            let sum = (t.hear + t.think + t.voice).max(1) as f32;
            pen.rect(PAD, 182.0, span, 2.0, LINE, 1.0);
            let mut x = PAD;
            let voice = if t.barged { AMBER } else { CYAN };
            for (ms, rgb, alpha) in [(t.hear, ICE, 0.9), (t.think, CYAN, 1.0), (t.voice, voice, 0.55)] {
                let len = span * ms as f32 / sum;
                if len >= 1.0 {
                    pen.rect(x, 182.0, (len - 2.0).max(1.0), 2.0, rgb, alpha);
                }
                x += len;
            }
            let third = span / 3.0;
            for (i, (name, ms)) in [("Hear", t.hear), ("Think", t.think), ("Voice", t.voice)]
                .into_iter()
                .enumerate()
            {
                let x = PAD + third * i as f32;
                let end = pen.say(name, x, 204.0, READOUT, FAINT);
                pen.say(&format!("{ms} ms"), end + 6.0, 204.0, READOUT, PAPER);
            }
        }
    }

    // The last thing she did.
    match &ai.action {
        None => {
            pen.say("No actions yet", PAD, 240.0, SMALL, FAINT);
        }
        Some(a) => {
            let rgb = match a.ok {
                Some(true) => CYAN,
                Some(false) => BAD,
                None => ICE,
            };
            pen.dot(PAD + 3.0, 236.0, 3.0, rgb);
            let end = pen.say(&a.name, PAD + 14.0, 240.0, READOUT, PAPER);
            let detail = if a.ok.is_none() { "running" } else { a.text.as_str() };
            pen.say_fit(detail, end + 10.0, right, 240.0, SMALL, DIM);
        }
    }

    // What she runs on.
    pen.rect(PAD, 256.0, right - PAD, 1.0, LINE, 1.0);
    let servers = match info.servers {
        0 => "No servers".to_string(),
        1 => "1 server".to_string(),
        n => format!("{n} servers"),
    };
    let at = pen.say_right(&servers, right, 278.0, READOUT, DIM);
    let value = PAD + 70.0;
    pen.say("Model", PAD, 278.0, READOUT, FAINT);
    pen.say_fit(&info.model, value, at - 12.0, 278.0, SMALL, DIM);
    pen.say("Hearing", PAD, 298.0, READOUT, FAINT);
    pen.say(info.hearing, value, 298.0, SMALL, DIM);
    let voice_at = PAD + 160.0;
    let end = pen.say("Voice", voice_at, 298.0, READOUT, FAINT);
    pen.say_fit(&info.voice, end + 10.0, right, 298.0, SMALL, DIM);
}

/// The right panel: the time, the machine's load, its power and network, and
/// the next reminders.
fn draw_machine(pen: &mut Pen, m: &Machine, now_ms: i64) {
    use crate::remind::{local_parts, MONTHS, WEEKDAYS};
    let (w, h) = SYS_SIZE;
    let right = w - PAD;
    pen.panel(w, h);

    let (_, month, day, hour, min, weekday) = local_parts(now_ms);
    pen.say(&format!("{hour:02}:{min:02}"), PAD - 2.0, 74.0, CLOCK, PAPER);
    let date = format!("{} {day} {}", WEEKDAYS[weekday], MONTHS[month as usize - 1]);
    pen.say(&date, PAD, 100.0, READOUT, DIM);

    for (y, name, frac) in [(140.0, "CPU", m.cpu_shown), (166.0, "RAM", m.mem_shown)] {
        pen.say(name, PAD, y, READOUT, FAINT);
        pen.say_right(&format!("{:.0}%", frac * 100.0), right, y, READOUT, PAPER);
        pen.bar(PAD + 44.0, y - 4.0, right - PAD - 44.0 - 44.0, frac, CYAN);
    }

    if let Some((pct, charging)) = m.battery {
        let end = pen.say("Battery", PAD, 204.0, READOUT, FAINT);
        let end = pen.say(&format!("{pct}%"), end + 8.0, 204.0, READOUT, PAPER);
        if charging {
            pen.say("charging", end + 8.0, 204.0, READOUT, DIM);
        }
    }
    let (net, rgb) = if m.online { ("Online", CYAN) } else { ("Offline", BAD) };
    let at = pen.say_right(net, right, 204.0, READOUT, rgb);
    pen.dot(at - 9.0, 200.0, 3.0, rgb);

    pen.rect(PAD, 222.0, right - PAD, 1.0, LINE, 1.0);
    pen.say("Next", PAD, 248.0, READOUT, FAINT);
    if m.reminders.is_empty() {
        pen.say("No reminders set", PAD, 272.0, SMALL, FAINT);
    }
    let (_, _, today, ..) = local_parts(now_ms);
    for (i, (due, text)) in m.reminders.iter().enumerate() {
        let y = 272.0 + 22.0 * i as f32;
        let (_, _, d, h, mi, wd) = local_parts(*due);
        let when = if d == today && due - now_ms < 86_400_000 {
            format!("{h:02}:{mi:02}")
        } else {
            format!("{} {h:02}:{mi:02}", &WEEKDAYS[wd][..3])
        };
        let end = pen.say(&when, PAD, y, READOUT, CYAN);
        let at = pen.say_right(&countdown(due - now_ms), right, y, READOUT, DIM);
        pen.say_fit(text, end + 12.0, at - 10.0, y, SMALL, PAPER);
    }
}

// ---------------------------------------------------------------- window --

/// Opens both widgets, on their own thread.
///
/// Hidden, without closing, by `IRA_WIDGETS=off` in the settings window.
pub fn spawn(ui: &crate::ui::Ui) {
    let ai = Arc::new(Mutex::new(Ai { live: true, ..Ai::default() }));

    let mut events = ui.subscribe();
    let feed = ai.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match events.recv().await {
                Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
                    Ok(v) => {
                        if let Ok(mut ai) = feed.lock() {
                            ai.apply(&v);
                        }
                    }
                    Err(e) => tracing::debug!("widgets could not read an event: {e}"),
                },
                // Fell behind and missed some. What is on the panel is still
                // the best guess there is; the next event corrects it.
                Err(RecvError::Lagged(_)) => continue,
                // The sender is gone, which means IRA is on her way out.
                Err(RecvError::Closed) => break,
            }
        }
        if let Ok(mut ai) = feed.lock() {
            ai.live = false;
        }
    });

    let ui = ui.clone();
    std::thread::spawn(move || {
        if let Err(e) = run(ai, ui) {
            // A panel that will not open is a missing panel, not a broken IRA.
            tracing::error!("corner widgets unavailable: {e}");
        }
    });
}

struct Hud {
    windows: [windows_sys::Win32::Foundation::HWND; 2],
    ai_canvas: Canvas,
    sys_canvas: Canvas,
    ai: Arc<Mutex<Ai>>,
    ui: crate::ui::Ui,
    text: Text,
    info: Info,
    machine: Machine,
    scale: f32,
    shown: bool,
    /// Something has the whole screen; see [`screen_taken`]. Checked once a
    /// second rather than every frame.
    taken: bool,
    ticks: u32,
}

/// Whether something has the whole screen: a full-screen app or game, or
/// Windows' presentation mode. The orb and the widgets both get out of its way,
/// and come back when it is gone.
///
/// This is what Windows itself uses to hold back notifications. There is no
/// equivalent for screen sharing: a meeting app sharing the screen does not
/// tell anyone else it is doing it.
pub(crate) fn screen_taken() -> bool {
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    };
    let mut state = 0;
    // SAFETY: an out-parameter that outlives the call.
    let hr = unsafe { SHQueryUserNotificationState(&mut state) };
    hr >= 0 && matches!(state, QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
}

/// Makes the windows this thread creates per-monitor DPI aware, so they are
/// drawn at the display's real resolution -- not drawn at 96 DPI and blown up
/// by Windows, which is blurry -- and are told when the scale changes.
///
/// Per thread rather than per process: the orb's thread and this one want it,
/// and nothing else in IRA has a window to change.
pub(crate) fn per_monitor_dpi() {
    use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    // SAFETY: a documented constant; a failure leaves the thread as it was,
    // which is DPI-unaware and stretched -- the old behaviour, not a crash.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// A canvas per panel at `scale`.
fn canvases(
    left: windows_sys::Win32::Foundation::HWND,
    right: windows_sys::Win32::Foundation::HWND,
    scale: f32,
) -> anyhow::Result<(Canvas, Canvas)> {
    let px = |(w, h): (f32, f32)| ((w * scale).round() as u32, (h * scale).round() as u32);
    let (aw, ah) = px(AI_SIZE);
    let (sw, sh) = px(SYS_SIZE);
    Ok((Canvas::new(left, aw, ah)?, Canvas::new(right, sw, sh)?))
}

thread_local! {
    static HUD: std::cell::RefCell<Option<Hud>> = const { std::cell::RefCell::new(None) };
}

/// The window class, UTF-16 for `RegisterClassW`.
const CLASS: &[u16] = &[b'I' as u16, b'R' as u16, b'A' as u16, b'H' as u16, b'u' as u16, b'd' as u16, 0];
const TIMER: usize = 1;

fn run(ai: Arc<Mutex<Ai>>, ui: crate::ui::Ui) -> anyhow::Result<()> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, SetTimer, TranslateMessage, MSG,
        WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    };

    let text = Text::new().ok_or_else(|| anyhow::anyhow!("the widgets' font would not load"))?;
    per_monitor_dpi();

    // SAFETY: a class registered with a static name and a window procedure of
    // the required signature, then two windows created from it. Every handle
    // is checked before it is used.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance as _,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            // No brush: every pixel comes from the bitmap, as with the orb.
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS.as_ptr(),
        };
        RegisterClassW(&class);

        // Layered and topmost like the orb, and also TRANSPARENT: the orb is a
        // button, these are not, so every click goes to what is underneath.
        let make = || {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                CLASS.as_ptr(),
                CLASS.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance as _,
                std::ptr::null_mut(),
            )
        };
        let (left, right) = (make(), make());
        if left.is_null() || right.is_null() {
            return Err(anyhow::anyhow!("widget windows would not open"));
        }

        let dpi = GetDpiForWindow(left);
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let (ai_canvas, sys_canvas) = canvases(left, right, scale)?;
        settle(left, right, scale);

        HUD.with(|cell| {
            *cell.borrow_mut() = Some(Hud {
                windows: [left, right],
                ai_canvas,
                sys_canvas,
                ai,
                info: Info::read(&ui),
                ui,
                text,
                machine: Machine::default(),
                scale,
                shown: false,
                taken: screen_taken(),
                ticks: 0,
            });
        });

        // Painted before they are shown, so nothing flashes; `tick` shows them.
        tick();
        SetTimer(left, TIMER, 1000 / FPS, None);
        tracing::info!("widgets up");

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Draws a frame of both panels, or hides them if the setting says so or
/// something has the whole screen.
fn tick() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE};
    HUD.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(hud) = cell.as_mut() else {
            return;
        };
        hud.ticks = hud.ticks.wrapping_add(1);
        if hud.ticks.is_multiple_of(FPS) {
            hud.taken = screen_taken();
        }
        let wanted = crate::settings::get("IRA_WIDGETS").as_deref() != Some("off") && !hud.taken;
        if !wanted {
            if hud.shown {
                // SAFETY: windows this thread created.
                hud.windows.iter().for_each(|&w| unsafe {
                    ShowWindow(w, SW_HIDE);
                });
                hud.shown = false;
            }
            return;
        }

        hud.machine.sample();
        if hud.ticks.is_multiple_of(FPS) {
            hud.info = Info::read(&hud.ui);
        }
        let ai = hud.ai.lock().map(|a| a.clone()).unwrap_or_default();
        let now = crate::audit::now_ms();

        draw_ai(&mut Pen { p: &mut hud.ai_canvas.pixmap, text: &mut hud.text, u: hud.scale }, &ai, &hud.info);
        hud.ai_canvas.present();
        draw_machine(&mut Pen { p: &mut hud.sys_canvas.pixmap, text: &mut hud.text, u: hud.scale }, &hud.machine, now);
        hud.sys_canvas.present();

        if !hud.shown {
            // SAFETY: windows this thread created.
            hud.windows.iter().for_each(|&w| unsafe {
                ShowWindow(w, SW_SHOWNOACTIVATE);
            });
            hud.shown = true;
        }
    });
}

/// Puts the panels in the top corners of the work area -- below a taskbar at
/// the top of the screen, if that is where it is.
fn settle(left: windows_sys::Win32::Foundation::HWND, right: windows_sys::Win32::Foundation::HWND, scale: f32) {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    // SAFETY: valid handles; `cbSize` set as the API requires, and the struct is
    // read only after a non-zero return.
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromWindow(left, MONITOR_DEFAULTTONEAREST), &mut info) == 0 {
            return;
        }
        let work = info.rcWork;
        let m = (MARGIN * scale) as i32;
        let size = |(w, h): (f32, f32)| ((w * scale).round() as i32, (h * scale).round() as i32);
        let (aw, ah) = size(AI_SIZE);
        let (sw, sh) = size(SYS_SIZE);
        SetWindowPos(left, std::ptr::null_mut(), work.left + m, work.top + m, aw, ah, SWP_NOZORDER | SWP_NOACTIVATE);
        SetWindowPos(right, std::ptr::null_mut(), work.right - sw - m, work.top + m, sw, sh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

unsafe extern "system" fn wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_SETTINGCHANGE,
        WM_TIMER,
    };
    match msg {
        WM_TIMER => {
            tick();
            0
        }
        // The display's scale changed: new canvases at the new size, then the
        // corners again, since the margins scale too. Only the first window
        // acts, so the pair is rebuilt once. Settled outside the borrow,
        // because moving a window sends messages back to this procedure.
        WM_DPICHANGED => {
            let scale = (wparam & 0xffff) as f32 / 96.0;
            let pair = HUD.with(|cell| {
                let mut cell = cell.try_borrow_mut().ok()?;
                let hud = cell.as_mut().filter(|h| h.windows[0] == hwnd)?;
                let [left, right] = hud.windows;
                match canvases(left, right, scale) {
                    Ok((a, s)) => {
                        hud.ai_canvas = a;
                        hud.sys_canvas = s;
                        hud.scale = scale;
                        Some((left, right))
                    }
                    Err(e) => {
                        tracing::error!("widgets could not follow the new scale: {e}");
                        None
                    }
                }
            });
            if let Some((left, right)) = pair {
                settle(left, right, scale);
                tick();
            }
            0
        }
        // A resolution change, or the taskbar moving: the corners moved too.
        // Only the first window acts, so the pair settles once.
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            HUD.with(|cell| {
                if let Ok(cell) = cell.try_borrow() {
                    if let Some(hud) = cell.as_ref().filter(|h| h.windows[0] == hwnd) {
                        settle(hud.windows[0], hud.windows[1], hud.scale);
                    }
                }
            });
            // SAFETY: forwarding the message exactly as it arrived.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fitting by a fake measure -- one unit a character -- so the cutting is
    /// checked without a font.
    #[test]
    fn long_text_is_cut_and_wrapped_to_its_room() {
        let ten = |s: &str| s.chars().count() <= 10;
        assert_eq!(fit("short", ten), "short");
        // Ten characters including the ellipsis, and no more.
        assert_eq!(fit("a much longer line", ten), "a much lo…");

        let lines = wrap("you have three things tomorrow at ten", 2, ten);
        assert_eq!(lines, vec!["you have", "three thi…"]);
        assert_eq!(wrap("one two", 2, ten), vec!["one two"]);
        assert!(wrap("", 2, ten).is_empty());
        // A word longer than the line is cut rather than spilling over.
        assert_eq!(wrap("supercalifragilistic", 2, ten), vec!["supercali…"]);
    }

    /// Events fold into what the panel shows, the way the loop sends them.
    #[test]
    fn events_fold_into_the_panel() {
        let mut ai = Ai { live: true, ..Ai::default() };
        let apply = |ai: &mut Ai, j: &str| ai.apply(&serde_json::from_str(j).unwrap());

        apply(&mut ai, r#"{"kind":"reply","text":"old answer"}"#);
        apply(&mut ai, r#"{"kind":"heard","text":"what's on tomorrow"}"#);
        assert_eq!(ai.heard, "what's on tomorrow");
        assert!(ai.reply.is_empty(), "a new question clears the old answer");

        apply(&mut ai, r#"{"kind":"tool","name":"open","args":"{}"}"#);
        assert_eq!(ai.action.as_ref().unwrap().ok, None);
        apply(&mut ai, r#"{"kind":"result","name":"open","ok":true,"text":"\nOpened Spotify\nmore"}"#);
        assert_eq!(
            ai.action,
            Some(Action { name: "open".into(), ok: Some(true), text: "Opened Spotify".into() })
        );

        apply(&mut ai, r#"{"kind":"state","name":"confirming"}"#);
        apply(&mut ai, r#"{"kind":"confirm","question":"Add that to your calendar?"}"#);
        assert_eq!(ai.label(), ("Asking you", AMBER));
        apply(&mut ai, r#"{"kind":"answered","yes":true}"#);
        assert!(ai.asking.is_none());

        apply(&mut ai, r#"{"kind":"speaking","on":true}"#);
        assert_eq!(ai.label().0, "Speaking", "speaking outranks the state");

        apply(
            &mut ai,
            r#"{"kind":"turn","id":3,"total_ms":900,"stt_ms":200,"ttft_ms":500,"tts_ms":200,"tool_ms":0,"tools":0,"barged":true}"#,
        );
        assert_eq!(ai.turn, Some(Times { hear: 200, think: 500, voice: 200, total: 900, barged: true }));

        ai.live = false;
        assert_eq!(ai.label(), ("Stopped", GREY), "a stopped IRA must not look busy");
    }

    #[test]
    fn cpu_and_countdowns_read_right() {
        // 100 ticks passed, 25 of them idle: three quarters busy.
        assert_eq!(busy((0, 0, 0), (25, 60, 40)), Some(0.75));
        assert_eq!(busy((5, 5, 5), (5, 5, 5)), None, "no time passed, no reading");

        assert_eq!(countdown(30_000), "now");
        assert_eq!(countdown(12 * 60_000), "12 min");
        assert_eq!(countdown(120 * 60_000), "2 h");
        assert_eq!(countdown(125 * 60_000), "2 h 5 min");
        assert_eq!(countdown(3 * 86_400_000), "3 d");
    }

    /// The font loads and actually puts ink down: a panel drawn with words in
    /// it has pixels in the text colour, not just the lens and the brackets.
    #[test]
    fn the_panels_draw_their_words() {
        let mut text = Text::new().expect("Archivo loads");
        assert!(text.width("IRA", MARK, 1.0) > 10.0);

        let (w, h) = AI_SIZE;
        let mut pixmap = tiny_skia::Pixmap::new(w as u32, h as u32).unwrap();
        let ai = Ai { live: true, heard: "what's on tomorrow".into(), ..Ai::default() };
        draw_ai(&mut Pen { p: &mut pixmap, text: &mut text, u: 1.0 }, &ai, &Info::default());

        // The "IRA" mark, top left, in near-white.
        let ink = (24..60)
            .flat_map(|x| (26..40).map(move |y| (x, y)))
            .filter(|&(x, y)| pixmap.pixel(x, y).unwrap().red() > 150)
            .count();
        assert!(ink > 15, "the mark drew {ink} bright pixels");
    }
}
