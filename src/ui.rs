//! The companion screen: a page IRA serves on loopback, fed by server-sent
//! events.
//!
//! A browser rather than a window. Scrolling, selection, copy, zoom and a
//! light/dark theme all come free, it costs no dependency and no second build
//! pipeline, and it is cross-platform without effort. The trade is that it is a
//! tab you leave open rather than an overlay that appears -- if that turns out
//! to matter, an overlay consumes exactly this event stream and nothing else
//! changes.
//!
//! Hand-rolled over `tokio::net`, because two routes and one content type is
//! less code than wiring up a web framework to serve them.
//!
//! Nothing here may affect the loop. A closed tab, a stalled reader, a browser
//! that never connects: all of it is a dropped socket and no more.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};

const DEFAULT_PORT: u16 = 8180;
/// Events replayed to a page that connects mid-conversation, so opening it
/// shows what just happened rather than an empty screen.
const BACKLOG: usize = 200;

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    State { name: &'static str },
    /// Sound is or is not leaving the speaker. Not a state: `Holding` covers
    /// thinking and speaking as one because barge-in must be armed across
    /// both, and splitting it would put that guarantee at risk to light a lamp.
    /// This rides alongside instead, and only the orb listens to it.
    Speaking { on: bool },
    Heard { text: String },
    Reply { text: String },
    Tool { name: String, args: String },
    Result { name: String, ok: bool, text: String },
    Confirm { question: String },
    Answered { yes: bool },
    Failed { what: String },
    Turn {
        id: u64,
        total_ms: u64,
        stt_ms: u64,
        ttft_ms: u64,
        tts_ms: u64,
        tool_ms: u64,
        tools: u64,
        barged: bool,
    },
}

#[derive(Clone)]
pub struct Ui {
    tx: broadcast::Sender<String>,
    watchers: Arc<AtomicUsize>,
    backlog: Arc<Mutex<VecDeque<String>>>,
    /// `POST /talk`. Opens the floor, or interrupts if IRA is speaking.
    talk: mpsc::Sender<()>,
    /// The port being served, so `POST /talk` can tell its own page from
    /// someone else's.
    port: u16,
    /// Whether the listener actually bound. `IRA_UI=off` and a port already in
    /// use both leave this false, and both mean there is no settings page to
    /// point a window at.
    served: bool,
}

impl Ui {
    /// A screen nobody can watch: events go nowhere and `watchers` stays zero.
    /// What `IRA_UI=off` produces, and what tests use.
    pub fn disabled() -> Self {
        let (tx, _) = broadcast::channel(256);
        // A sender whose receiver is already gone: pressing talk does nothing.
        let (talk, _) = mpsc::channel(1);
        Self {
            tx,
            watchers: Arc::new(AtomicUsize::new(0)),
            backlog: Arc::new(Mutex::new(VecDeque::with_capacity(BACKLOG))),
            talk,
            port: DEFAULT_PORT,
            served: false,
        }
    }

    /// Starts the server unless `IRA_UI=off`. `IRA_UI=<port>` moves it.
    ///
    /// The receiver carries presses of the talk control.
    pub async fn start() -> (Self, mpsc::Receiver<()>) {
        let (talk, talk_rx) = mpsc::channel(4);
        let setting = std::env::var("IRA_UI").unwrap_or_default();
        let port: u16 = setting.parse().unwrap_or(DEFAULT_PORT);
        let mut ui = Self { talk, port, ..Self::disabled() };

        if setting == "off" {
            tracing::info!("screen disabled");
            return (ui, talk_rx);
        }

        // Loopback only. This carries a live transcript of everything said in
        // the room; it does not belong on a network interface.
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => {
                // The port asked for may be 0, meaning "any". Recording what
                // was actually bound is not bookkeeping: `same_origin` compares
                // against it, and a page served on 51000 that believes it is on
                // 0 refuses its own talk button.
                let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                tracing::info!("screen at http://127.0.0.1:{port}");
                ui.served = true;
                ui.port = port;
                let ui2 = ui.clone();
                tokio::spawn(async move {
                    while let Ok((sock, _)) = listener.accept().await {
                        let ui = ui2.clone();
                        tokio::spawn(async move { serve(sock, ui).await });
                    }
                });
            }
            // A port already in use must not stop IRA answering questions.
            Err(e) => tracing::error!("screen unavailable on port {port}: {e}"),
        }
        (ui, talk_rx)
    }

    /// The port the screen is on, or `None` if it never bound. What the
    /// settings window needs: it is a webview onto a route, and without the
    /// route there is nothing to show.
    ///
    /// Gated because that window is the only caller and is Windows-only;
    /// without this it is dead code everywhere else, which CI treats as an
    /// error and is right to.
    #[cfg(windows)]
    pub fn served(&self) -> Option<u16> {
        self.served.then_some(self.port)
    }

    /// The event stream, for a consumer inside this process. The orb reads
    /// this instead of connecting to the socket the browser uses.
    ///
    /// Subscribing is deliberately *not* watching: `watchers` counts pages that
    /// can show a transcript, and the orb shows a colour. An orb that counted
    /// would have IRA saying she had put the detail on screen with no page open
    /// -- the lie P1 removed.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// Presses the talk control, as the button on the page does.
    pub fn press_talk(&self) {
        // try_send: a second press while the first is still queued is the same
        // press, and this must never block the caller on the loop.
        let _ = self.talk.try_send(());
    }

    /// How many pages are watching. Zero means IRA must not claim to have put
    /// anything on screen -- see `llm::system`.
    pub fn watchers(&self) -> usize {
        self.watchers.load(Ordering::Relaxed)
    }

    pub fn send(&self, event: Event) {
        let Ok(json) = serde_json::to_string(&event) else {
            return;
        };
        if let Ok(mut b) = self.backlog.lock() {
            if b.len() == BACKLOG {
                b.pop_front();
            }
            b.push_back(json.clone());
        }
        // Err just means nobody is watching.
        let _ = self.tx.send(json);
    }
}

/// Whether this `POST /talk` came from IRA's own page.
///
/// Binding to loopback is not the defence it looks like: `POST /talk` opens the
/// microphone, and any page in any tab can post to 127.0.0.1 cross-origin. It
/// needs no reply, so CORS never blocks it -- the request has already had its
/// effect by the time the browser discards the response.
///
/// A browser labels its own page's fetch `same-origin` and anything else
/// `cross-site`. curl sends neither header, which is why the documented
/// `curl -X POST` still works: this refuses browsers that say they are
/// elsewhere, not clients that say nothing.
async fn same_origin<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, port: u16) -> bool {
    let mine = [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")];
    let mut line = String::new();
    let mut ok = true;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            // A truncated request is not one to act on.
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        let Some((name, value)) = line.trim_end().split_once(':') else {
            // The blank line ends the headers; anything else malformed is not
            // a header we were looking for.
            break;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "sec-fetch-site" => ok &= value.eq_ignore_ascii_case("same-origin"),
            // The fallback for a browser too old to send Sec-Fetch-Site.
            "origin" => ok &= mine.iter().any(|m| m == value),
            _ => {}
        }
    }
    ok
}

async fn serve(mut sock: TcpStream, ui: Ui) {
    let mut line = String::new();
    let (read, mut write) = sock.split();
    let mut reader = BufReader::new(read);
    if reader.read_line(&mut line).await.is_err() {
        return;
    }

    if line.starts_with("POST /talk") {
        if !same_origin(&mut reader, ui.port).await {
            tracing::warn!("cross-site talk press refused");
            let _ = write
                .write_all(b"HTTP/1.1 403 Forbidden
Connection: close

")
                .await;
            return;
        }
        ui.press_talk();
        let _ = write
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }

    // Saving changes what IRA runs with and stores a key, so it is guarded
    // exactly as `POST /talk` is: a browser that says it is somewhere else is
    // refused, and a client that says nothing -- curl -- is not.
    if line.starts_with("POST /settings") {
        if !same_origin(&mut reader, ui.port).await {
            tracing::warn!("cross-site settings save refused");
            let _ = write.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n").await;
            return;
        }
        let answer = save_setting(&mut reader).await;
        reply_json(&mut write, if answer.starts_with("{\"error") { 400 } else { 200 }, &answer).await;
        return;
    }

    if line.starts_with("GET /settings/state") {
        reply_json(&mut write, 200, &settings_state()).await;
        return;
    }

    if !line.starts_with("GET /events") {
        let body = if line.starts_with("GET /settings") { SETTINGS } else { PAGE }.as_bytes();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = write.write_all(head.as_bytes()).await;
        let _ = write.write_all(body).await;
        return;
    }

    let mut rx = ui.subscribe();
    let backlog: Vec<String> = ui
        .backlog
        .lock()
        .map(|b| b.iter().cloned().collect())
        .unwrap_or_default();

    if write
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
        )
        .await
        .is_err()
    {
        return;
    }

    ui.watchers.fetch_add(1, Ordering::Relaxed);
    for json in backlog {
        if write.write_all(format!("data: {json}\n\n").as_bytes()).await.is_err() {
            ui.watchers.fetch_sub(1, Ordering::Relaxed);
            return;
        }
    }

    loop {
        match rx.recv().await {
            Ok(json) => {
                if write.write_all(format!("data: {json}\n\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            // A slow page misses events rather than slowing the loop down.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(n, "screen fell behind");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    ui.watchers.fetch_sub(1, Ordering::Relaxed);
}


/// What the settings page is allowed to know.
///
/// A secret's value is never in here -- only whether one is stored. The page
/// cannot show you a key you have forgotten, and neither can anything else that
/// can reach this port.
fn settings_state() -> String {
    let fields: Vec<Value> = crate::settings::FIELDS
        .iter()
        .map(|f| {
            let mut o = json!({
                "name": f.name,
                "about": f.about,
                "secret": f.secret,
                "set": crate::settings::is_set(f.name),
            });
            if !f.secret {
                o["value"] = json!(crate::settings::get(f.name).unwrap_or_default());
            }
            o
        })
        .collect();
    serde_json::to_string(&fields).unwrap_or_else(|_| "[]".into())
}

/// Reads a `{name, value}` body and saves it. Returns the JSON to answer with.
async fn save_setting<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> String {
    let error = |what: String| json!({ "error": what }).to_string();

    // `same_origin` has already consumed the headers, so what is left is the
    // body -- and its length is not knowable from here. These bodies are two
    // short strings, so a cap is both a limit and a frame: read up to it, and
    // stop at the closing brace.
    let mut body = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match tokio::io::AsyncReadExt::read(reader, &mut byte).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                body.push(byte[0]);
                if byte[0] == b'}' || body.len() >= 8192 {
                    break;
                }
            }
        }
    }

    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return error("that was not a settings change".into());
    };
    let (Some(name), Some(value)) = (v["name"].as_str(), v["value"].as_str()) else {
        return error("a settings change needs a name and a value".into());
    };
    match crate::settings::set(name, value) {
        Ok(()) => json!({ "saved": name }).to_string(),
        // The message can name the setting and the reason; it must never quote
        // the value back, because for half of these the value is a key.
        Err(e) => error(format!("{name} was not saved: {e}")),
    }
}

async fn reply_json<W: tokio::io::AsyncWrite + Unpin>(write: &mut W, status: u16, body: &str) {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = write.write_all(head.as_bytes()).await;
    let _ = write.write_all(body.as_bytes()).await;
}

/// The settings window's page: what IRA is configured with, and a box to
/// change each of it.
///
/// Served rather than built into the window, because the window is a webview
/// and this is the thing it shows. Same origin as `POST /settings`, so the
/// cross-site guard lets its saves through for the same reason it lets the
/// talk button's presses through.
const SETTINGS: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>IRA — settings</title>
<style>
  :root {
    --bg:#f4f6f8; --panel:#fff; --line:#d9dee5; --ink:#191c22;
    --soft:#3e454f; --muted:#626c7a; --accent:#9e540c;
    --ok:#1d6b50; --warn:#9e2c2c;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg:#131519; --panel:#1a1d23; --line:#2c313a; --ink:#e7eaee;
      --soft:#c3c9d2; --muted:#949daa; --accent:#de9b48;
      --ok:#4eae87; --warn:#e07373;
    }
  }
  * { box-sizing:border-box; }
  body {
    margin:0; background:var(--bg); color:var(--ink);
    font:14px/1.55 "Segoe UI",system-ui,sans-serif;
  }
  header {
    position:sticky; top:0; display:flex; align-items:center; gap:12px;
    padding:12px 20px; background:var(--panel); border-bottom:1px solid var(--line);
  }
  h1 { margin:0; font-size:13px; letter-spacing:.16em; text-transform:uppercase; }
  #note { margin-left:auto; font-size:12px; color:var(--muted); }
  #note.ok { color:var(--ok); } #note.bad { color:var(--warn); }
  main { max-width:640px; margin:0 auto; padding:20px; }
  .row { margin-bottom:18px; }
  label {
    display:block; font:11px ui-monospace,Consolas,monospace; letter-spacing:.08em;
    color:var(--soft); margin-bottom:4px;
  }
  .about { font-size:12px; color:var(--muted); margin:0 0 6px; }
  .line { display:flex; gap:8px; }
  input {
    flex:1; min-width:0; padding:7px 10px; border-radius:5px;
    border:1px solid var(--line); background:var(--panel); color:var(--ink);
    font:13px ui-monospace,Consolas,monospace;
  }
  input:focus-visible { outline:2px solid var(--accent); outline-offset:1px; }
  button {
    font:12px ui-monospace,Consolas,monospace; letter-spacing:.06em;
    cursor:pointer; color:var(--panel); background:var(--accent);
    border:1px solid var(--accent); border-radius:5px; padding:0 14px;
  }
  button.quiet { background:transparent; color:var(--muted); border-color:var(--line); }
  button:active { filter:brightness(.85); }
  .state { font-size:11px; color:var(--muted); margin-top:4px; min-height:1.2em; }
  .state.set { color:var(--ok); }
  footer {
    color:var(--muted); font-size:12px; border-top:1px solid var(--line);
    margin-top:26px; padding-top:14px;
  }
  code { font-family:ui-monospace,Consolas,monospace; color:var(--soft); }
</style>
</head>
<body>
<header>
  <h1>IRA — settings</h1>
  <span id="note">applies immediately</span>
</header>
<main id="form"></main>
<script>
const form = document.getElementById('form');
const note = document.getElementById('note');
let fields = [];

function say(text, cls) {
  note.textContent = text;
  note.className = cls || '';
}

// A secret is never sent back, so the box starts empty whether or not one is
// stored and the line underneath says which. Typing replaces it; Clear removes
// it. There is deliberately no way to read one back out.
function draw() {
  form.innerHTML = '';
  for (const f of fields) {
    const row = document.createElement('div');
    row.className = 'row';

    const label = document.createElement('label');
    label.textContent = f.name;
    label.htmlFor = f.name;

    const about = document.createElement('p');
    about.className = 'about';
    about.textContent = f.about;

    const line = document.createElement('div');
    line.className = 'line';

    const input = document.createElement('input');
    input.id = f.name;
    input.type = f.secret ? 'password' : 'text';
    input.autocomplete = 'off';
    input.spellcheck = false;
    input.value = f.secret ? '' : (f.value || '');
    input.placeholder = f.secret
      ? (f.set ? 'stored — type to replace' : 'not set')
      : 'not set';

    const save = document.createElement('button');
    save.textContent = 'Save';
    save.onclick = () => send(f.name, input.value);

    const clear = document.createElement('button');
    clear.textContent = 'Clear';
    clear.className = 'quiet';
    clear.onclick = () => { input.value = ''; send(f.name, ''); };

    input.onkeydown = e => { if (e.key === 'Enter') send(f.name, input.value); };

    const state = document.createElement('div');
    state.className = 'state' + (f.set ? ' set' : '');
    state.id = 'state-' + f.name;
    state.textContent = f.set
      ? (f.secret ? 'stored in the Windows Credential Manager' : 'set')
      : 'not set';

    line.append(input, save, clear);
    row.append(label, about, line, state);
    form.append(row);
  }

  const foot = document.createElement('footer');
  foot.innerHTML =
    'Keys go to the Windows Credential Manager, never to a file. Everything else '
    + 'is written to <code>ira.local.toml</code>. Saved values take effect on the '
    + 'next thing IRA says — nothing needs restarting.';
  form.append(foot);
}

async function load() {
  try {
    const r = await fetch('/settings/state');
    fields = await r.json();
    draw();
  } catch (e) {
    say('could not read settings', 'bad');
  }
}

async function send(name, value) {
  say('saving…');
  try {
    const r = await fetch('/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, value }),
    });
    const answer = await r.json();
    if (!r.ok || answer.error) {
      say(answer.error || ('save failed: ' + r.status), 'bad');
      return;
    }
    say(value.trim() ? (name + ' saved') : (name + ' cleared'), 'ok');
    await load();
  } catch (e) {
    say('save failed: ' + e, 'bad');
  }
}

load();
</script>
</body>
</html>
"##;

const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>IRA</title>
<style>
  :root {
    --bg:#f4f6f8; --panel:#fff; --line:#d9dee5; --ink:#191c22;
    --soft:#3e454f; --muted:#626c7a; --accent:#9e540c;
    --you:#1d6b50; --warn:#9e2c2c;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg:#131519; --panel:#1a1d23; --line:#2c313a; --ink:#e7eaee;
      --soft:#c3c9d2; --muted:#949daa; --accent:#de9b48;
      --you:#4eae87; --warn:#e07373;
    }
  }
  * { box-sizing:border-box; }
  body {
    margin:0; background:var(--bg); color:var(--ink);
    font:15px/1.6 "Segoe UI",system-ui,sans-serif;
  }
  header {
    position:sticky; top:0; display:flex; align-items:center; gap:12px;
    padding:12px 20px; background:var(--panel); border-bottom:1px solid var(--line);
  }
  h1 { margin:0; font-size:15px; letter-spacing:.16em; text-transform:uppercase; }
  #state {
    font:12px ui-monospace,Consolas,monospace; letter-spacing:.08em;
    text-transform:uppercase; color:var(--accent);
    border:1px solid var(--accent); border-radius:3px; padding:2px 8px;
  }
  #live { margin-left:auto; font-size:12px; color:var(--muted); }
  #talk {
    font:12px ui-monospace,Consolas,monospace; letter-spacing:.08em;
    text-transform:uppercase; cursor:pointer; color:var(--panel);
    background:var(--accent); border:1px solid var(--accent);
    border-radius:3px; padding:4px 12px;
  }
  #talk:active { filter:brightness(.85); }
  #talk:focus-visible { outline:2px solid var(--ink); outline-offset:2px; }
  main { max-width:820px; margin:0 auto; padding:20px; }
  .row { margin-bottom:14px; }
  .who {
    font:11px ui-monospace,Consolas,monospace; letter-spacing:.1em;
    text-transform:uppercase; color:var(--muted); margin-bottom:3px;
  }
  .what {
    background:var(--panel); border:1px solid var(--line); border-radius:5px;
    padding:10px 14px; white-space:pre-wrap; overflow-wrap:anywhere;
  }
  .you .who { color:var(--you); }
  .you .what { border-left:3px solid var(--you); }
  .ira .what { border-left:3px solid var(--accent); }
  .tool .what, .result .what {
    font:13px/1.55 ui-monospace,Consolas,monospace; color:var(--soft);
  }
  .ask .what { border-left:3px solid var(--warn); font-weight:600; }
  .bad .what { border-left:3px solid var(--warn); color:var(--warn); }
  .turn {
    display:flex; flex-wrap:wrap; gap:6px 14px;
    font:11px ui-monospace,Consolas,monospace; color:var(--muted);
    border-top:1px dashed var(--line); padding-top:8px; margin:18px 0 22px;
  }
  .turn b { color:var(--soft); font-weight:600; }
  .empty { color:var(--muted); text-align:center; padding:60px 0; }
</style>
</head>
<body>
<header>
  <h1>IRA</h1>
  <span id="state">idle</span>
  <button id="talk" title="Open the floor, or interrupt">talk</button>
  <span id="live">connecting…</span>
</header>
<main id="log"><div class="empty">Waiting for the wake word.</div></main>
<script>
const log = document.getElementById('log');
const stateEl = document.getElementById('state');
const liveEl = document.getElementById('live');
let empty = true;

function row(cls, who, what) {
  if (empty) { log.innerHTML = ''; empty = false; }
  const d = document.createElement('div');
  d.className = 'row ' + cls;
  const w = document.createElement('div');
  w.className = 'who'; w.textContent = who;
  const b = document.createElement('div');
  b.className = 'what'; b.textContent = what;
  d.append(w, b);
  log.append(d);
  // Only follow if already at the bottom, so reading scrollback is not fought.
  const near = window.innerHeight + window.scrollY >= document.body.offsetHeight - 120;
  if (near) window.scrollTo(0, document.body.scrollHeight);
}

document.getElementById('talk').onclick = () =>
  fetch('/talk', { method: 'POST' });

const src = new EventSource('/events');
src.onopen = () => liveEl.textContent = 'live';
src.onerror = () => liveEl.textContent = 'disconnected';
src.onmessage = (e) => {
  const m = JSON.parse(e.data);
  switch (m.kind) {
    case 'state':    stateEl.textContent = m.name; break;
    case 'heard':    row('you', 'you', m.text); break;
    case 'reply':    row('ira', 'ira', m.text); break;
    case 'tool':     row('tool', 'calling ' + m.name, m.args); break;
    case 'result':   row('result', m.ok ? 'result' : 'tool failed', m.text); break;
    case 'confirm':  row('ask', 'asking', m.question); break;
    case 'answered': row('ask', 'you', m.yes ? 'yes' : 'no'); break;
    case 'failed':   row('bad', 'failed', m.what); break;
    case 'turn': {
      if (empty) { log.innerHTML = ''; empty = false; }
      const d = document.createElement('div');
      d.className = 'turn';
      const parts = [
        ['turn', m.id], ['total', m.total_ms + ' ms'], ['stt', m.stt_ms + ' ms'],
        ['ttft', m.ttft_ms + ' ms'], ['tts', m.tts_ms + ' ms'],
      ];
      if (m.tools) parts.push(['tools', m.tools], ['tool time', m.tool_ms + ' ms']);
      if (m.barged) parts.push(['', 'interrupted']);
      d.innerHTML = parts.map(([k, v]) => k ? `<span><b>${k}</b> ${v}</span>` : `<span>${v}</span>`).join('');
      log.append(d);
      const near = window.innerHeight + window.scrollY >= document.body.offsetHeight - 120;
      if (near) window.scrollTo(0, document.body.scrollHeight);
      break;
    }
  }
};
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// `IRA_UI` is process-wide and cargo runs tests in parallel: without this
    /// one test's `off` is another's start-up. A failure here would be
    /// intermittent and would look like a bug in the server.
    /// Async, because it is held across the `await` on `Ui::start`.
    static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// With nothing watching, IRA must not be told it has a screen.
    #[tokio::test]
    async fn no_watchers_before_anyone_connects() {
        let _env = ENV.lock().await;
        std::env::set_var("IRA_UI", "off");
        let (ui, _talk) = Ui::start().await;
        assert_eq!(ui.watchers(), 0);
        // Sending into the void is a no-op, not an error.
        ui.send(Event::Heard { text: "hello".into() });
        assert_eq!(ui.watchers(), 0);
        std::env::remove_var("IRA_UI");
    }

    /// The page has to survive a browser's parser; a stray backtick or an
    /// unbalanced brace in the raw string would only show up at runtime.
    #[test]
    fn the_page_is_whole() {
        assert!(PAGE.starts_with("<!doctype html>"));
        assert!(PAGE.trim_end().ends_with("</html>"));
        assert!(PAGE.contains("new EventSource('/events')"));
        assert_eq!(
            PAGE.matches("<script>").count(),
            PAGE.matches("</script>").count()
        );
    }



    /// The orb switches on `kind`, exactly as the page does.
    #[test]
    fn speaking_serialises_with_the_kind_the_orb_switches_on() {
        let json = serde_json::to_string(&Event::Speaking { on: true }).unwrap();
        assert!(json.contains(r#""kind":"speaking""#), "got {json}");
        assert!(json.contains(r#""on":true"#), "got {json}");
    }

    /// The microphone is one cross-origin POST away from any open tab, and
    /// loopback does not stand in the way of it.
    #[tokio::test]
    async fn a_cross_site_page_cannot_press_talk() {
        let req = |extra: &str| {
            std::io::Cursor::new(format!("Host: 127.0.0.1:8180
{extra}
").into_bytes())
        };

        assert!(!same_origin(&mut req("Sec-Fetch-Site: cross-site
"), 8180).await);
        assert!(!same_origin(&mut req("Origin: https://evil.example
"), 8180).await);
        // Its own page, and a different port's page, are not the same thing.
        assert!(!same_origin(&mut req("Origin: http://127.0.0.1:3000
"), 8180).await);

        assert!(same_origin(&mut req("Sec-Fetch-Site: same-origin
"), 8180).await);
        assert!(same_origin(&mut req("Origin: http://127.0.0.1:8180
"), 8180).await);
        // curl, which is the documented way to bind a real hotkey.
        assert!(same_origin(&mut req(""), 8180).await);
    }

    #[test]
    fn events_serialise_with_a_kind_the_page_switches_on() {
        let json = serde_json::to_string(&Event::Heard { text: "hi".into() }).unwrap();
        assert!(json.contains(r#""kind":"heard""#), "got {json}");
        let json = serde_json::to_string(&Event::Result {
            name: "clock".into(),
            ok: true,
            text: "now".into(),
        })
        .unwrap();
        assert!(json.contains(r#""kind":"result""#), "got {json}");
    }
}
