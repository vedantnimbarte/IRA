//! The corner widget: what IRA is doing, at the top left, while she is doing it.
//!
//! [decisions/0023](../docs/decisions/0023-the-corners-show-what-she-is-doing.md),
//! narrowed by [0024](../docs/decisions/0024-the-corner-shows-only-during-a-turn.md).
//! The orb is a light and says one thing. This says the rest, in words: what
//! she heard and said, how long the last turn took and where the time went,
//! what she last did, what she is running on. It opens when she wakes and
//! closes [`LINGER`] after she goes back to idle; the rest of the time the
//! corner is yours.
//!
//! **The same kind of window as the orb.** Layered, topmost, drawn with
//! tiny-skia and pushed with the orb's own [`Canvas`]. A webview cannot be
//! transparent (0013), and this sits over your work. It is also
//! click-through -- `WS_EX_TRANSPARENT` -- so it never takes a click meant for
//! the window underneath. Text is drawn with swash, from the same Archivo file
//! the settings page loads, at the website's widths.
//!
//! **It reads the same broadcast as the orb.** Subscribing is not watching,
//! exactly as there: IRA does not tell the model there is a screen because
//! this is up, so she never claims to have put the detail on one.
//!
//! **Hidden from the settings window.** `IRA_WIDGETS`, read on every frame, so
//! hiding it is immediate and needs no restart.
//!
//! Every failure here is a missing panel and never a broken IRA, on the same
//! terms as `orb.rs`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::orb::Canvas;

/// Logical size of the panel, before the display's scale.
const AI_SIZE: (f32, f32) = (380.0, 316.0);
/// From the corner of the work area, the same as the orb's.
const MARGIN: f32 = 24.0;
/// Inside the panel.
const PAD: f32 = 24.0;
/// Redraws a second. Nothing on the panel changes faster than a state does,
/// and a quarter of a second is quicker than anyone reads one.
const FPS: u32 = 4;
/// How long the panel stays up once she is idle again: long enough to read the
/// two lines of her reply after she finishes saying them.
const LINGER: Duration = Duration::from_secs(8);

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

    /// Whether she is in a turn: woken, thinking, speaking or asking. What
    /// decides that the panel is up.
    fn busy(&self) -> bool {
        self.live && (self.speaking || matches!(self.state.as_str(), "listening" | "holding" | "confirming"))
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
        // Only ever seen at the start of her first turn: she is listening.
        pen.say("Go ahead.", PAD, 84.0, BODY, DIM);
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

// ---------------------------------------------------------------- window --

/// Opens the widget, on its own thread.
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
            tracing::error!("corner widget unavailable: {e}");
        }
    });
}

struct Hud {
    window: windows_sys::Win32::Foundation::HWND,
    canvas: Canvas,
    ai: Arc<Mutex<Ai>>,
    ui: crate::ui::Ui,
    text: Text,
    info: Info,
    scale: f32,
    shown: bool,
    /// The last frame she was in a turn, so the panel lingers after it.
    busy_at: Option<Instant>,
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

/// The panel's canvas at `scale`.
fn canvas(window: windows_sys::Win32::Foundation::HWND, scale: f32) -> anyhow::Result<Canvas> {
    let (w, h) = AI_SIZE;
    Canvas::new(window, (w * scale).round() as u32, (h * scale).round() as u32)
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
    // the required signature, then a window created from it. Every handle
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
        // button, this is not, so every click goes to what is underneath.
        let window = CreateWindowExW(
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
            );
        if window.is_null() {
            return Err(anyhow::anyhow!("widget window would not open"));
        }

        let dpi = GetDpiForWindow(window);
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let canvas = canvas(window, scale)?;
        settle(window, scale);

        HUD.with(|cell| {
            *cell.borrow_mut() = Some(Hud {
                window,
                canvas,
                ai,
                info: Info::read(&ui),
                ui,
                text,
                scale,
                shown: false,
                busy_at: None,
                taken: screen_taken(),
                ticks: 0,
            });
        });

        tick();
        SetTimer(window, TIMER, 1000 / FPS, None);
        tracing::info!("widgets up");

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Draws a frame of the panel while she is in a turn and for [`LINGER`] after,
/// and hides it otherwise -- or whenever the setting says so or something has
/// the whole screen.
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
        let ai = hud.ai.lock().map(|a| a.clone()).unwrap_or_default();
        if ai.busy() {
            hud.busy_at = Some(Instant::now());
        }
        let wanted = crate::settings::get("IRA_WIDGETS").as_deref() != Some("off")
            && !hud.taken
            && hud.busy_at.is_some_and(|t| t.elapsed() < LINGER);
        if !wanted {
            if hud.shown {
                // SAFETY: a window this thread created.
                unsafe { ShowWindow(hud.window, SW_HIDE) };
                hud.shown = false;
            }
            return;
        }

        if hud.ticks.is_multiple_of(FPS) {
            hud.info = Info::read(&hud.ui);
        }
        draw_ai(&mut Pen { p: &mut hud.canvas.pixmap, text: &mut hud.text, u: hud.scale }, &ai, &hud.info);
        hud.canvas.present();

        if !hud.shown {
            // Painted above before it is shown, so nothing flashes.
            // SAFETY: a window this thread created.
            unsafe { ShowWindow(hud.window, SW_SHOWNOACTIVATE) };
            hud.shown = true;
        }
    });
}

/// Puts the panel in the top-left corner of the work area -- below a taskbar at
/// the top of the screen, if that is where it is.
fn settle(window: windows_sys::Win32::Foundation::HWND, scale: f32) {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    // SAFETY: valid handles; `cbSize` set as the API requires, and the struct is
    // read only after a non-zero return.
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut info) == 0 {
            return;
        }
        let work = info.rcWork;
        let m = (MARGIN * scale) as i32;
        let size = |(w, h): (f32, f32)| ((w * scale).round() as i32, (h * scale).round() as i32);
        let (w, h) = size(AI_SIZE);
        SetWindowPos(window, std::ptr::null_mut(), work.left + m, work.top + m, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
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
        // The display's scale changed: a new canvas at the new size, then the
        // corner again, since the margin scales too. Settled outside the
        // borrow, because moving a window sends messages back to this procedure.
        WM_DPICHANGED => {
            let scale = (wparam & 0xffff) as f32 / 96.0;
            let rebuilt = HUD.with(|cell| {
                let mut cell = cell.try_borrow_mut().ok()?;
                let hud = cell.as_mut()?;
                match canvas(hud.window, scale) {
                    Ok(c) => {
                        hud.canvas = c;
                        hud.scale = scale;
                        Some(())
                    }
                    Err(e) => {
                        tracing::error!("widget could not follow the new scale: {e}");
                        None
                    }
                }
            });
            if rebuilt.is_some() {
                settle(hwnd, scale);
                tick();
            }
            0
        }
        // A resolution change, or the taskbar moving: the corner moved too.
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            HUD.with(|cell| {
                if let Ok(cell) = cell.try_borrow() {
                    if let Some(hud) = cell.as_ref() {
                        settle(hud.window, hud.scale);
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

    /// The panel is up for a turn and only for a turn.
    #[test]
    fn only_a_turn_puts_the_panel_up() {
        let mut ai = Ai { live: true, ..Ai::default() };
        assert!(!ai.busy(), "nothing has happened yet");
        for state in ["listening", "holding", "confirming"] {
            ai.state = state.into();
            assert!(ai.busy(), "{state} is a turn");
        }
        ai.state = "idle".into();
        assert!(!ai.busy());
        ai.speaking = true;
        assert!(ai.busy(), "still talking after the loop went idle");
        ai.live = false;
        assert!(!ai.busy(), "a stopped IRA has no turn to show");
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
