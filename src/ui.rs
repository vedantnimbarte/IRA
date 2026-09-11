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
    /// What the settings page needs to change the running IRA, rather than just
    /// a stored value. Filled in once, after the tool registry exists.
    admin: Arc<std::sync::OnceLock<Admin>>,
    /// The last state the loop announced, so `GET /state` can answer without
    /// the caller having to watch the stream from the beginning.
    state: Arc<Mutex<&'static str>>,
}

/// The settings page's reach into the running process.
///
/// A `Weak` on purpose: the registry holds a `Ui` so it can report every call
/// to the screen, and a strong handle back would be a cycle that leaks the
/// registry for the life of the process.
pub struct Admin {
    pub host: std::sync::Weak<crate::tool::Host>,
    /// Words for IRA to say when she next has the floor. The same queue a
    /// finished background job lands on, because it is the same problem:
    /// something outside the conversation has news, and interrupting a turn to
    /// deliver it would be worse than waiting.
    pub say: mpsc::Sender<(String, bool)>,
    /// The same channel a mutating tool asks down. Spawning a process someone
    /// typed into a web page is at least as much of a write as sending an
    /// email, so it goes through the gate that already exists.
    pub confirm: mpsc::Sender<crate::tool::Confirm>,
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
            admin: Arc::new(std::sync::OnceLock::new()),
            state: Arc::new(Mutex::new("idle")),
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

    /// Hands the page its reach into the running process. Called once, after
    /// the tool registry is built -- which is necessarily after the screen is
    /// serving, since the registry reports to it.
    ///
    /// Until this is set the page can still read and save settings; it just
    /// cannot connect a server, which it says rather than failing quietly.
    pub fn set_admin(&self, admin: Admin) {
        if self.admin.set(admin).is_err() {
            tracing::error!("the settings page was given its admin handle twice");
        }
    }

    /// The running tool registry, if there is one.
    ///
    /// `None` before `set_admin`, and `None` if the registry has been dropped
    /// -- which only happens as the process ends. Every caller treats it as
    /// "IRA is not running to change", not as an error.
    fn host(&self) -> Option<Arc<crate::tool::Host>> {
        self.admin.get().and_then(|a| a.host.upgrade())
    }

    /// Asks the user out loud, and waits for a spoken yes or no.
    ///
    /// The same gate a mutating tool goes through, for the same reason: the
    /// answer decides whether IRA runs a program. **Anything that is not an
    /// explicit yes is a no** -- a refusal, silence, an unparsable answer, and
    /// a loop that is not listening at all, which is what a test or
    /// `IRA_UI=off` produces.
    async fn ask(&self, question: String) -> bool {
        let Some(admin) = self.admin.get() else {
            return false;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if admin.confirm.send(crate::tool::Confirm { question, reply: tx }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Cap on what one `POST /say` may queue. IRA reads this aloud, and there
    /// is no way to skip a sentence except by interrupting her.
    const SAY_MAX: usize = 500;

    /// Reads a `{text}` body and queues it to be spoken when IRA next has the
    /// floor. Never interrupts: a build that finished is not more important
    /// than the sentence someone is in the middle of.
    fn queue_say(&self, body: &[u8]) -> String {
        let Some(admin) = self.admin.get() else {
            return error("IRA is not running yet");
        };
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            return error("that was not something to say");
        };
        let text = v["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            return error("nothing to say");
        }
        if text.chars().count() > Self::SAY_MAX {
            return error(format!("too long -- {} characters at most", Self::SAY_MAX));
        }
        // try_send, not send: this route must never wait on the loop. A full
        // queue means IRA is already behind on things to say, and the honest
        // answer is to refuse rather than to pile on.
        match admin.say.try_send((text.to_string(), true)) {
            Ok(()) => json!({ "queued": text.chars().count() }).to_string(),
            Err(_) => error("she already has more to say than she can get through"),
        }
    }

    /// What IRA is doing, for a script that would rather poll than hold the
    /// event stream open.
    fn state_json(&self) -> String {
        let host = self.host();
        let (tools, jobs) = match host.as_deref() {
            Some(h) => {
                let mut names: Vec<String> = h.specs().into_iter().map(|s| s.name).collect();
                names.sort();
                (names, h.running())
            }
            None => (Vec::new(), 0),
        };
        json!({
            "state": self.state.lock().map(|s| *s).unwrap_or("unknown"),
            "watchers": self.watchers(),
            "running": host.is_some(),
            "tools": tools,
            "jobs": jobs,
            "skills": crate::skills::count(),
        })
        .to_string()
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
        // Remembered on the way past, so a poll of `GET /state` does not have
        // to replay the backlog to work out where the loop got to.
        if let Event::State { name } = &event {
            if let Ok(mut s) = self.state.lock() {
                *s = name;
            }
        }
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
    read_headers(reader, port).await.same_origin
}

/// What the routes need to know about a request, once its headers are read.
struct Headers {
    /// No header claimed a different origin.
    same_origin: bool,
    /// A header actually said where this came from. `same_origin` is true for
    /// a request that said nothing at all, which is deliberate for `/talk` and
    /// not good enough for a route that can spawn a process.
    stated_origin: bool,
    length: usize,
}

/// Reads the headers, answering the two questions anything past them needs:
/// whether the request is IRA's own page, and how long the body is.
///
/// The length matters. Without it the body had to be framed by its first
/// closing brace, which is correct for `{name, value}` and silently truncates
/// anything nested -- a server's `headers` object would have ended the read
/// halfway through itself.
async fn read_headers<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    port: u16,
) -> Headers {
    let mine = [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")];
    let mut line = String::new();
    let mut length = 0usize;
    let mut stated = false;
    let mut ok = true;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            // A truncated request is not one to act on.
            Ok(0) | Err(_) => {
                return Headers { same_origin: false, stated_origin: false, length: 0 }
            }
            Ok(_) => {}
        }
        let Some((name, value)) = line.trim_end().split_once(':') else {
            // The blank line ends the headers; anything else malformed is not
            // a header we were looking for.
            break;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "sec-fetch-site" => {
                stated = true;
                ok &= value.eq_ignore_ascii_case("same-origin");
            }
            // The fallback for a browser too old to send Sec-Fetch-Site.
            "origin" => {
                stated = true;
                ok &= mine.iter().any(|m| m == value);
            }
            // Capped here rather than trusted: the number is the client's, and
            // a huge one must not become a huge allocation.
            "content-length" => length = value.parse().unwrap_or(0).min(BODY_MAX),
            _ => {}
        }
    }
    Headers { same_origin: ok, stated_origin: stated, length }
}

/// Cap on a request body. A skill is 16 KB of text and a server block is a few
/// hundred bytes, so this is generous for both and still bounded.
const BODY_MAX: usize = 64 * 1024;

/// Reads exactly `length` bytes of body.
async fn read_body<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, length: usize) -> Vec<u8> {
    let mut body = vec![0u8; length];
    match tokio::io::AsyncReadExt::read_exact(reader, &mut body).await {
        Ok(_) => body,
        // A truncated body is not one to act on.
        Err(_) => Vec::new(),
    }
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
    // Where a provider sends the browser back after a sign-in. Not guarded by
    // origin, and cannot be: the whole point is that it arrives as a top-level
    // navigation from somewhere else. What guards it is the `state` -- issued
    // by us, held in memory, good for exactly one callback -- so a forged
    // redirect matches nothing.
    if line.starts_with("GET /oauth/callback") {
        let answer = oauth_callback(&ui, &line).await;
        let body = format!(
            "<!doctype html><meta charset=utf-8>\
             <title>IRA</title>\
             <style>body{{font:15px/1.6 system-ui;margin:12vh auto;max-width:30rem;padding:0 1.5rem}}</style>\
             <p>{answer}</p><p>You can close this tab.</p>"
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = write.write_all(head.as_bytes()).await;
        let _ = write.write_all(body.as_bytes()).await;
        return;
    }

    // Something outside the conversation has news. Guarded like `POST /talk`
    // rather than like the admin route -- a client that states no origin is
    // allowed, because being scriptable from curl, a CI job or a build hook is
    // the entire point. It queues words; it cannot run anything.
    if line.starts_with("POST /say") {
        let h = read_headers(&mut reader, ui.port).await;
        if !h.same_origin {
            tracing::warn!("cross-site say refused");
            let _ = write.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n").await;
            return;
        }
        let body = read_body(&mut reader, h.length).await;
        let answer = ui.queue_say(&body);
        reply_json(&mut write, if answer.starts_with("{\"error") { 400 } else { 200 }, &answer).await;
        return;
    }

    // What IRA is doing, for something that wants to poll rather than hold the
    // event stream open. Read-only, so no guard beyond loopback -- the same
    // reasoning as `GET /events`, which already carries far more.
    if line.starts_with("GET /state") {
        reply_json(&mut write, 200, &ui.state_json()).await;
        return;
    }

    // Servers, their per-tool policy, and skills. One route with an `op` rather
    // than six: they share a guard, a body reader and an answer shape, and six
    // near-identical branches of an HTTP parser is the thing worth avoiding.
    if line.starts_with("POST /settings/admin") {
        let h = read_headers(&mut reader, ui.port).await;
        // Stricter than `POST /talk` and `POST /settings` on purpose. Those let
        // a client that states no origin through, so the documented
        // `curl -X POST` keeps working. This route can make IRA spawn a
        // process someone typed into a web page, and a request that says
        // nothing about where it came from is not evidence of anything -- so
        // here the page has to actually say it is the page.
        if !h.same_origin || !h.stated_origin {
            tracing::warn!("admin change refused: not from IRA's own page");
            let _ = write.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n").await;
            return;
        }
        let body = read_body(&mut reader, h.length).await;
        let answer = admin_op(&ui, &body).await;
        reply_json(&mut write, if answer.starts_with("{\"error") { 400 } else { 200 }, &answer).await;
        return;
    }

    if line.starts_with("POST /settings") {
        let h = read_headers(&mut reader, ui.port).await;
        if !h.same_origin {
            tracing::warn!("cross-site settings save refused");
            let _ = write.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n").await;
            return;
        }
        let body = read_body(&mut reader, h.length).await;
        let answer = save_setting(&body);
        reply_json(&mut write, if answer.starts_with("{\"error") { 400 } else { 200 }, &answer).await;
        return;
    }

    if line.starts_with("GET /settings/state") {
        reply_json(&mut write, 200, &settings_state(&ui)).await;
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
fn settings_state(ui: &Ui) -> String {
    let fields: Vec<Value> = crate::settings::FIELDS
        .iter()
        .map(|f| {
            let mut o = json!({
                "name": f.name,
                "label": f.label,
                "group": f.group,
                "about": f.about,
                "empty": f.when_empty(),
                "secret": f.secret,
                "set": crate::settings::is_set(f.name),
                "store": f.store(),
            });
            if !f.secret {
                o["value"] = json!(crate::settings::get(f.name).unwrap_or_default());
            }
            o
        })
        .collect();
    let groups: Vec<Value> = crate::settings::GROUPS
        .iter()
        .map(|g| json!({ "id": g.id, "title": g.title, "about": g.about }))
        .collect();
    // Servers as configured, each carrying the tools it is currently offering
    // so the policy switches have something real to switch. A server that is
    // off, or that failed to connect, has an empty list and says so.
    // Which tools are actually registered right now, by the server that
    // brought them. What is configured and what is connected are different
    // questions, and the page has to be able to show both.
    let host = ui.host();
    let live: Vec<(String, String)> = host
        .as_deref()
        .map(crate::tool::Host::by_server)
        .unwrap_or_default();
    let servers: Vec<Value> = crate::db::servers()
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            let tools: Vec<Value> = live
                .iter()
                .filter(|(server, _)| *server == s.name)
                .map(|(_, tool)| {
                    let p = s.policy(tool);
                    json!({
                        "name": tool,
                        "exposed": p.exposed,
                        // Null rather than false: the page has to be able to
                        // show "nobody has said" differently from "safe", or
                        // the gate looks disarmed when it is armed.
                        "mutates": p.mutates,
                        "latency": p.latency,
                        "confirm": p.confirm,
                    })
                })
                .collect();
            let env: Vec<Value> = crate::db::env_names(&s.name)
                .unwrap_or_default()
                .into_iter()
                .map(|n| {
                    let set = crate::settings::secret::read(
                        &crate::settings::secret::env_key(&s.name, &n),
                    )
                    .ok()
                    .flatten()
                    .is_some();
                    // Whether a value is stored, never the value itself -- the
                    // same rule the six provider keys follow.
                    json!({ "name": n, "set": set })
                })
                .collect();
            json!({
                "name": s.name,
                "transport": s.transport,
                "env": env,
                "signed_in": crate::db::oauth_get(&s.name).ok().flatten().is_some(),
                "command": s.command,
                "args": s.args,
                "url": s.url,
                "headers": s.headers,
                "enabled": s.enabled,
                "connected": !tools.is_empty(),
                "tools": tools,
            })
        })
        .collect();

    let skills: Vec<Value> = crate::skills::index()
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "enabled": s.enabled,
                "path": s.path,
            })
        })
        .collect();

    serde_json::to_string(&json!({
        "groups": groups,
        "fields": fields,
        "servers": servers,
        "skills": skills,
        // Without a registry the page is an editor for a database and should
        // say so, rather than promising changes that only land on a restart.
        "live": host.is_some(),
    }))
        .unwrap_or_else(|_| "{}".into())
}

/// Reads a `{name, value}` body and saves it. Returns the JSON to answer with.
fn save_setting(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return error("that was not a settings change");
    };
    let (Some(name), Some(value)) = (v["name"].as_str(), v["value"].as_str()) else {
        return error("a settings change needs a name and a value");
    };
    match crate::settings::set(name, value) {
        Ok(()) => json!({ "saved": name }).to_string(),
        // The message can name the setting and the reason; it must never quote
        // the value back, because for half of these the value is a key.
        Err(e) => error(format!("{name} was not saved: {e}")),
    }
}

fn error(what: impl std::fmt::Display) -> String {
    json!({ "error": what.to_string() }).to_string()
}

/// Everything the window can change about servers and skills.
///
/// One entry point with an `op`, and every arm ends by returning fresh state,
/// so the page never has to guess what its change did -- particularly for
/// "connect", where the answer is a list of tools it did not have before.
async fn admin_op(ui: &Ui, body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return error("that was not a change");
    };
    let op = v["op"].as_str().unwrap_or_default();
    let name = v["name"].as_str().unwrap_or_default().trim().to_string();

    let outcome: anyhow::Result<Option<String>> = match op {
        "server_save" => save_server(ui, &v).await.map(Some),
        "server_delete" => delete_server(ui, &name).map(|()| None),
        "server_toggle" => toggle_server(ui, &name, v["on"].as_bool().unwrap_or(false))
            .await
            .map(Some),
        "tool_policy" => save_policy(ui, &v).await.map(|()| None),
        "env_save" => save_env(&v).map(Some),
        // Returns a URL rather than state: the page has to open it, because
        // signing in happens at the provider's site and not here.
        "oauth_begin" => {
            return match begin_sign_in(ui, &name).await {
                Ok(url) => json!({ "open": url }).to_string(),
                Err(e) => error(format!("{e:#}")),
            }
        }
        "oauth_forget" => crate::oauth::forget(&name).map(|()| None),
        "env_delete" => delete_env(&v).map(|()| None),
        // Call one tool by hand, before you are mid-sentence discovering it
        // does not work. The result is shown raw -- this is a wiring check, not
        // a conversation, and a model's phrasing of a failure would hide it.
        "tool_try" => {
            return match try_tool(ui, &v).await {
                Ok(text) => json!({ "tried": v["tool"], "text": text }).to_string(),
                Err(e) => error(format!("{e:#}")),
            }
        }
        "skill_save" => crate::skills::write(
            &name,
            v["description"].as_str().unwrap_or_default(),
            v["body"].as_str().unwrap_or_default(),
        )
        .map(|()| None),
        "skill_delete" => crate::skills::delete(&name).map(|()| None),
        "skill_toggle" => {
            crate::skills::enable(&name, v["on"].as_bool().unwrap_or(false)).map(|()| None)
        }
        // Read-only, and the one op that answers with something other than
        // state: the page asks for one body when a skill is opened for editing,
        // rather than being sent every body it might never look at.
        "skill_body" => {
            return match crate::skills::body_of(&name) {
                Ok(text) => json!({ "name": name, "text": text }).to_string(),
                Err(e) => error(e),
            }
        }
        other => return error(format!("unknown change: {other}")),
    };

    match outcome {
        Ok(note) => {
            // Skills come and go, so the tool that serves them has to as well.
            if let Some(host) = ui.host() {
                crate::skills::sync_registry(&host);
            }
            let mut state: Value =
                serde_json::from_str(&settings_state(ui)).unwrap_or_else(|_| json!({}));
            state["note"] = json!(note);
            state.to_string()
        }
        Err(e) => error(format!("{e:#}")),
    }
}

/// Saves a server and connects it, so the model can use it without a restart.
///
/// A stdio server is a command IRA will spawn. That is a write in every sense
/// the confirmation gate was built for, and a bigger one than most tools make
/// -- so it is spoken aloud and needs an explicit yes, before anything is
/// stored. A refusal leaves nothing behind.
async fn save_server(ui: &Ui, v: &Value) -> anyhow::Result<String> {
    let s = server_from_json(v)?;

    if s.transport == "stdio" {
        let command = s.command.clone().unwrap_or_default();
        let question = format!(
            "Let me run {} whenever I start? Yes or no.",
            command.rsplit(['/', '\\']).next().unwrap_or(&command)
        );
        if !ui.ask(question).await {
            anyhow::bail!("not saved -- you said no, or nothing was listening to ask");
        }
    }

    crate::db::server_set(&s)?;
    if !s.enabled {
        if let Some(host) = ui.host() {
            host.remove_server(&s.name);
        }
        return Ok(format!("{} saved, and off.", s.name));
    }
    connect(ui, &s).await
}

/// Connects one server and swaps its tools into the running registry.
fn server_from_json(v: &Value) -> anyhow::Result<crate::db::Server> {
    let name = v["name"].as_str().unwrap_or_default().trim().to_string();
    // The name keys two tables and is shown to the model as part of every tool
    // it exposes, so it is held to the same shape a skill name is.
    let plain = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        anyhow::bail!("a server name is letters, digits, dashes and underscores");
    }

    let transport = v["transport"].as_str().unwrap_or_default().to_string();
    let command = v["command"].as_str().map(str::trim).filter(|c| !c.is_empty());
    let url = v["url"].as_str().map(str::trim).filter(|u| !u.is_empty());
    match transport.as_str() {
        "stdio" if command.is_none() => anyhow::bail!("a stdio server needs a command to run"),
        "http" if url.is_none() => anyhow::bail!("an http server needs a URL"),
        "stdio" | "http" => {}
        other => anyhow::bail!("transport is stdio or http, not {other:?}"),
    }

    Ok(crate::db::Server {
        name,
        transport,
        command: command.map(String::from),
        args: v["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        url: url.map(String::from),
        headers: v["headers"]
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        enabled: v["enabled"].as_bool().unwrap_or(true),
        tools: Default::default(),
    })
}

/// Connects, and reports in words the page can show. A failure here is not a
/// failure to save: the server stays configured so it can be fixed and retried,
/// which is why this returns a note rather than an error.
async fn connect(ui: &Ui, s: &crate::db::Server) -> anyhow::Result<String> {
    let Some(host) = ui.host() else {
        return Ok(format!("{} saved. It will connect when IRA restarts.", s.name));
    };
    // Re-read so the policy rows saved for this server are applied, rather than
    // the empty set that came in over the wire.
    let stored = crate::db::server_get(&s.name)?.unwrap_or_else(|| s.clone());
    match crate::mcp::connect_within_timeout(&stored).await {
        Ok(tools) => {
            let n = tools.len();
            host.set_server(&s.name, tools);
            Ok(match n {
                0 => format!("{} connected, and offers nothing.", s.name),
                1 => format!("{} connected. 1 tool.", s.name),
                n => format!("{} connected. {n} tools.", s.name),
            })
        }
        Err(e) => {
            host.remove_server(&s.name);
            Ok(format!("{} saved, but did not connect: {e:#}", s.name))
        }
    }
}

/// Starts a sign-in for one server and returns the URL to open.
async fn begin_sign_in(ui: &Ui, name: &str) -> anyhow::Result<String> {
    let s = crate::db::server_get(name)?
        .ok_or_else(|| anyhow::anyhow!("no such server: {name}"))?;
    let url = s
        .url
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow::anyhow!("only a server with a URL can sign in"))?;
    let scopes = crate::db::oauth_get(name)?.unwrap_or_default().scopes;
    let redirect = format!("http://127.0.0.1:{}/oauth/callback", ui.port);
    crate::oauth::begin(name, &url, &redirect, &scopes).await
}

/// Finishes a sign-in and connects the server it was for.
///
/// Returns the sentence shown in the tab the provider redirected. Deliberately
/// plain prose: whoever is reading it has just been bounced through two sites
/// and wants to know whether it worked.
async fn oauth_callback(ui: &Ui, request_line: &str) -> String {
    let query = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.split_once('?'))
        .map(|(_, q)| q)
        .unwrap_or_default();
    let param = |want: &str| {
        query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == want)
            .map(|(_, v)| crate::oauth::percent_decode(v))
    };

    // The provider says no by redirecting with an error, not by failing to
    // redirect, so this is the ordinary refusal path rather than an edge case.
    if let Some(e) = param("error") {
        let described = param("error_description").unwrap_or_default();
        tracing::warn!("sign-in refused: {e} {described}");
        return format!("The sign-in was refused: {e}. {described}");
    }

    let (Some(code), Some(state)) = (param("code"), param("state")) else {
        return "That callback was missing its code.".into();
    };

    let server = match crate::oauth::finish(&code, &state).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("sign-in did not complete: {e:#}");
            return format!("The sign-in did not complete: {e:#}");
        }
    };

    match crate::db::server_get(&server) {
        Ok(Some(s)) if s.enabled => match connect(ui, &s).await {
            Ok(note) => format!("Signed in. {note}"),
            Err(e) => format!("Signed in, but {server} did not connect: {e:#}"),
        },
        _ => format!("Signed in to {server}."),
    }
}

/// Records a variable a server is given, and puts its value in the keyring.
///
/// The name goes in the database and the value never does. An empty value
/// records the name without a value, which is how you add `GITHUB_TOKEN` now
/// and paste it in later; `mcp.rs` passes nothing at all in that case rather
/// than an empty string, which many servers read as "configured".
fn save_env(v: &Value) -> anyhow::Result<String> {
    let server = v["server"].as_str().unwrap_or_default();
    let name = v["name"].as_str().unwrap_or_default().trim();
    // An environment variable name, and nothing that could be a shell trick.
    let plain = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if server.is_empty() || !plain {
        anyhow::bail!("a variable name is letters, digits and underscores");
    }

    crate::db::env_add(server, name)?;
    let value = v["value"].as_str().unwrap_or_default();
    let key = crate::settings::secret::env_key(server, name);
    if value.is_empty() {
        // An empty box on an existing variable means "leave it alone", not
        // "erase it" -- clearing is the Remove button, which is unambiguous.
        return Ok(format!("{name} recorded for {server}."));
    }
    crate::settings::secret::write(&key, value)?;
    Ok(format!("{name} saved to the keyring. Reconnect {server} to use it."))
}

fn delete_env(v: &Value) -> anyhow::Result<()> {
    let server = v["server"].as_str().unwrap_or_default();
    let name = v["name"].as_str().unwrap_or_default();
    if server.is_empty() || name.is_empty() {
        anyhow::bail!("which variable, on which server?");
    }
    crate::settings::secret::delete(&crate::settings::secret::env_key(server, name))?;
    crate::db::env_delete(server, name)
}

/// Runs one tool directly, bypassing the model.
///
/// **And bypassing the confirmation gate**, deliberately: a person pressing
/// "Try it" on a named tool in their own settings window *is* the confirmation,
/// and asking out loud for a button they just pressed is theatre. The gate
/// exists because the model chose the tool; here the user did.
async fn try_tool(ui: &Ui, v: &Value) -> anyhow::Result<String> {
    let name = v["tool"].as_str().unwrap_or_default();
    let host = ui
        .host()
        .ok_or_else(|| anyhow::anyhow!("IRA is not running, so there is nothing to try"))?;
    let args: Value = match v["args"].as_str().map(str::trim).filter(|a| !a.is_empty()) {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| anyhow::anyhow!("those arguments are not JSON: {e}"))?,
        None => json!({}),
    };

    let tool = host
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no such tool: {name}"))?;
    let ctx = crate::tool::ToolCtx {
        transcript: String::new(),
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    // The tool's own budget still applies, so a hung server fails here the same
    // way it would fail inside a turn.
    let budget = tool.spec().latency.budget();
    match tokio::time::timeout(budget, tool.call(args, &ctx)).await {
        Ok(Ok(crate::tool::ToolOutcome::Answer(text))) => Ok(text),
        Ok(Ok(crate::tool::ToolOutcome::Started(id))) => {
            Ok(format!("Started as background job {id}."))
        }
        Ok(Ok(_)) => Ok("Done. It reported nothing back.".into()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!(
            "no answer within {}s",
            budget.as_secs().max(1)
        )),
    }
}

async fn toggle_server(ui: &Ui, name: &str, on: bool) -> anyhow::Result<String> {
    let mut s = crate::db::server_get(name)?
        .ok_or_else(|| anyhow::anyhow!("no such server: {name}"))?;
    s.enabled = on;
    crate::db::server_set(&s)?;
    if !on {
        if let Some(host) = ui.host() {
            host.remove_server(name);
        }
        return Ok(format!("{name} is off."));
    }
    // Turning a stdio server back on spawns it again, so it asks again.
    if s.transport == "stdio" {
        let command = s.command.clone().unwrap_or_default();
        if !ui
            .ask(format!(
                "Start {} again? Yes or no.",
                command.rsplit(['/', '\\']).next().unwrap_or(&command)
            ))
            .await
        {
            s.enabled = false;
            crate::db::server_set(&s)?;
            anyhow::bail!("left off -- you said no, or nothing was listening to ask");
        }
    }
    connect(ui, &s).await
}

fn delete_server(ui: &Ui, name: &str) -> anyhow::Result<()> {
    if let Some(host) = ui.host() {
        host.remove_server(name);
    }
    crate::mcp::forget(name)
}

/// Saves one tool's policy and reconnects the server it belongs to, because a
/// spec is built at connect time and `mutates` has to reach the registry to
/// mean anything.
async fn save_policy(ui: &Ui, v: &Value) -> anyhow::Result<()> {
    let server = v["server"].as_str().unwrap_or_default();
    let tool = v["tool"].as_str().unwrap_or_default();
    if server.is_empty() || tool.is_empty() {
        anyhow::bail!("a policy needs a server and a tool");
    }
    let latency = v["latency"].as_str().map(str::to_string);
    if let Some(l) = latency.as_deref() {
        if !matches!(l, "fast" | "slow" | "background") {
            anyhow::bail!("latency is fast, slow or background, not {l:?}");
        }
    }
    crate::db::tool_policy_set(
        server,
        tool,
        &crate::db::ToolPolicy {
            exposed: v["exposed"].as_bool().unwrap_or(true),
            // Absent stays absent: "nobody has said" and "someone said no" are
            // different, and only the first may default to asking.
            mutates: v["mutates"].as_bool(),
            latency,
            confirm: v["confirm"]
                .as_str()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(String::from),
        },
    )?;

    if let Some(s) = crate::db::server_get(server)? {
        if s.enabled {
            let _ = connect(ui, &s).await;
        }
    }
    Ok(())
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

/// The settings window's page.
///
/// Three places to be, and a rail down the left that says which one you are in:
/// the keys and endpoints IRA answers with, the MCP servers she calls tools
/// from, and the skills she reads. The rail is fixed and only the pane beside
/// it scrolls, so moving between them is one click from anywhere rather than a
/// scroll back to the top.
///
/// Within keys, hearing and answering are still two blocks in the order a turn
/// happens, because everything before transcription already runs on this
/// machine and that is what these six values actually divide into. The rail
/// groups them; it does not regroup them.
///
/// Servers and skills are grids of cards rather than stacked rows. A card is
/// the smallest thing that can answer "is this working" -- a status dot, what
/// it is doing, and where it comes from -- and a grid of them fits on one
/// screen where a stack of expandable rows did not. Opening one replaces the
/// grid with that server's own page: its form, what it is given, signing in,
/// and a block per tool. That is far too much to unfold inside a grid cell
/// without shoving every other card down the page.
///
/// The orb's own light is used exactly once, on the rail beside the section you
/// are in. It used to run down the side of every stage; one ornament that says
/// where you are is worth more than four that say nothing.
///
/// Values are set in a monospace face and nothing else is, because a key, a URL
/// or a command is read character by character and that is a legibility need
/// rather than a label style. No web fonts: IRA runs without a network and a
/// settings page that fetched a typeface would be the only part of her that did
/// not.
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
    --ground:#eef1f5; --rail:#e3e8f0; --surface:#fff; --sunk:#f5f7fa;
    --ink:#131820; --soft:#3f4956; --muted:#78849a; --faint:#9aa5b8;
    --line:#dfe4ec; --edge:#cdd5e2;
    --good:#0d7a68; --bad:#a72f3c; --focus:#2a6df4;
    --lift:0 6px 22px rgba(12,18,30,.12);
    /* The orb's own light, sampled from it. Used once, on the rail. */
    --thread:linear-gradient(180deg,#19a2fe,#7f9bfb,#cc9bfd,#fe84e4,#71fbf0);
    --sans:"Segoe UI Variable Text","Segoe UI Variable","Segoe UI",system-ui,sans-serif;
    --display:"Segoe UI Variable Display","Segoe UI Variable","Segoe UI",system-ui,sans-serif;
    --mono:"Cascadia Mono",Consolas,ui-monospace,monospace;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --ground:#0c1017; --rail:#080b11; --surface:#141a23; --sunk:#10151d;
      --ink:#e8ecf2; --soft:#b6c0cf; --muted:#7d8899; --faint:#5f6a7a;
      --line:#222a35; --edge:#2d3745;
      --good:#4fd6c4; --bad:#f08a94; --focus:#6ba2ff;
      --lift:0 8px 26px rgba(0,0,0,.45);
    }
  }
  * { box-sizing:border-box; }
  /* `.labelled` is a flex box, which outranks the `hidden` attribute the
     server form uses to put away the fields the other kind of server
     wants. Hiding wins. */
  [hidden] { display:none !important; }
  html, body { height:100%; }
  body {
    margin:0; background:var(--ground); color:var(--ink);
    font:400 14px/1.55 var(--sans);
    -webkit-font-smoothing:antialiased;
    display:grid; grid-template-columns:236px minmax(0,1fr);
  }

  /* --- the rail ------------------------------------------------------- */
  #nav {
    background:var(--rail); border-right:1px solid var(--line);
    padding:26px 14px 18px; overflow:auto;
    display:flex; flex-direction:column; gap:3px;
  }
  .brand {
    padding:2px 14px 22px; display:flex; flex-direction:column; gap:5px;
    font:500 11.5px/1 var(--sans); color:var(--muted);
  }
  .brand span { font:300 20px/1 var(--display); letter-spacing:-.015em; color:var(--ink); }
  .nav-item {
    position:relative; display:flex; align-items:center; gap:10px; width:100%;
    height:auto; padding:9px 12px 9px 14px; border:0; border-radius:8px; cursor:pointer;
    background:transparent; color:var(--soft);
    font:500 13.5px/1.4 var(--sans); text-align:left;
    transition:background .13s ease, color .13s ease;
  }
  .nav-item:hover { background:var(--sunk); color:var(--ink); filter:none; }
  .nav-item.on { background:var(--surface); color:var(--ink); }
  /* The one ornament, and it is carrying where you are. */
  .nav-item.on::before {
    content:""; position:absolute; left:0; top:8px; bottom:8px; width:3px;
    border-radius:3px; background:var(--thread);
  }
  .nav-count { margin-left:auto; font:400 11.5px/1 var(--mono); color:var(--faint); }
  .nav-item.on .nav-count { color:var(--muted); }

  /* --- the pane ------------------------------------------------------- */
  #main { overflow:auto; padding:34px 40px 60px; }
  .pane { max-width:880px; }
  .detail { max-width:580px; }
  .pane-head { display:flex; align-items:flex-start; gap:24px; margin:0 0 24px; }
  .pane-title { flex:1; min-width:0; }
  h1 { font:300 27px/1.2 var(--display); letter-spacing:-.02em; margin:0 0 7px; }
  .pane-title p, .detail-sub { margin:0; color:var(--muted); font-size:13.5px; max-width:58ch; }
  .detail-sub { margin:0 0 20px; }
  .back {
    height:auto; background:none; border:0; padding:4px 0; margin:0 0 16px;
    cursor:pointer; color:var(--muted); font:400 13px/1 var(--sans);
  }
  .back:hover { color:var(--ink); filter:none; }
  .empty { margin:18px 0 0; color:var(--muted); font-size:13px; max-width:52ch; }

  /* --- a block -------------------------------------------------------- */
  .block {
    background:var(--surface); border:1px solid var(--line); border-radius:12px;
    padding:22px 24px 8px; margin:0 0 16px;
  }
  .block > h2 { font:400 17px/1.3 var(--display); letter-spacing:-.01em; margin:0; }
  .block-about { margin:5px 0 0; color:var(--muted); font-size:12.5px; max-width:56ch; }

  /* --- a field -------------------------------------------------------- */
  .field { padding:20px 0 18px; border-bottom:1px solid var(--line); }
  .field:last-child { border-bottom:0; }
  .head { display:flex; align-items:baseline; gap:10px; flex-wrap:wrap; }
  .name { font-size:14px; font-weight:600; color:var(--ink); }
  /* The variable name, because this is what `doctor` and the docs call it. */
  .var { font:400 11.5px/1 var(--mono); color:var(--faint); }
  .about { margin:3px 0 0; color:var(--muted); font-size:12.5px; line-height:1.45; max-width:54ch; }

  .control { display:flex; gap:8px; margin-top:11px; align-items:stretch; }
  input {
    flex:1 1 auto; min-width:0; height:38px; padding:0 12px;
    border:1px solid var(--edge); border-radius:9px;
    background:var(--sunk); color:var(--ink);
    font:400 13px/1 var(--mono);
    transition:border-color .15s ease, background .15s ease;
  }
  input::placeholder { color:var(--faint); font-family:var(--sans); font-size:13px; }
  input:hover { border-color:var(--muted); }
  input:focus { outline:none; border-color:var(--focus); background:var(--ground); }
  input.saved { border-color:var(--good); }

  /* Actions arrive when you are working on a field, so six of them are not
     competing for attention while you read. A reserved column, so every box is
     the same width whether or not its field has something to clear. */
  .actions {
    display:flex; gap:8px; flex:0 0 152px; opacity:0; pointer-events:none;
    transform:translateX(-4px);
    transition:opacity .16s ease, transform .16s ease;
  }
  .field.busy .actions { opacity:1; pointer-events:auto; transform:none; }
  button {
    height:38px; padding:0 16px; border-radius:9px; cursor:pointer;
    font:600 13px/1 var(--sans);
    border:1px solid var(--ink); background:var(--ink); color:var(--ground);
    transition:filter .12s ease;
  }
  button.ghost { background:transparent; color:var(--muted); border-color:var(--edge); font-weight:400; }
  button:hover { filter:brightness(1.12); }
  button.ghost:hover { color:var(--ink); border-color:var(--muted); }
  button:focus-visible, input:focus-visible, textarea:focus-visible, select:focus-visible {
    outline:2px solid var(--focus); outline-offset:2px;
  }

  .status { margin:8px 0 0; font-size:12.5px; color:var(--muted); min-height:1.3em; }
  .status.is-set { color:var(--good); }
  .status.is-bad { color:var(--bad); }

  .foot {
    margin:22px 0 0; color:var(--faint); font-size:12.5px; line-height:1.55;
    max-width:60ch;
  }
  .foot code { font:400 12px/1 var(--mono); color:var(--muted); }

  /* --- the grids ------------------------------------------------------ */
  .grid {
    display:grid; grid-template-columns:repeat(auto-fill,minmax(218px,1fr));
    gap:14px; align-items:stretch;
  }
  .card {
    position:relative; display:flex; flex-direction:column; gap:7px;
    background:var(--surface); border:1px solid var(--line); border-radius:12px;
    padding:15px 16px 13px; min-height:134px;
    transition:border-color .14s ease;
  }
  .card:hover { border-color:var(--edge); }
  .card:focus-within { border-color:var(--focus); }
  .card-top { display:flex; align-items:center; gap:9px; }
  .dot { flex:none; width:7px; height:7px; border-radius:50%; background:var(--faint); }
  .dot.live { background:var(--good); }
  .dot.down { background:var(--bad); }
  /* The name opens the card, stretched over the whole tile -- so the switch is
     the only thing in it that does something else. */
  .card-open {
    flex:1; min-width:0; height:auto; padding:0; border:0; border-radius:0;
    background:none; color:var(--ink); cursor:pointer; text-align:left;
    font:600 14px/1.35 var(--sans);
    overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
  }
  .card-open:hover { filter:none; text-decoration:underline; }
  .card-open::after { content:""; position:absolute; inset:0; border-radius:12px; }
  .card .switch { position:relative; z-index:1; }
  .card-state { margin:0; font-size:12.5px; color:var(--soft); }
  .card-desc {
    margin:0; font-size:12.5px; color:var(--muted);
    display:-webkit-box; -webkit-line-clamp:3; -webkit-box-orient:vertical; overflow:hidden;
  }
  .card-where {
    margin:0; font:400 11.5px/1.45 var(--mono); color:var(--faint); overflow-wrap:anywhere;
    display:-webkit-box; -webkit-line-clamp:2; -webkit-box-orient:vertical; overflow:hidden;
  }
  .card-foot {
    margin:auto 0 0; padding-top:11px; border-top:1px solid var(--line);
    font-size:11.5px; color:var(--muted); overflow-wrap:anywhere;
  }
  .card-foot.path { font:400 11px/1.4 var(--mono); color:var(--faint); }

  /* --- rows, forms and switches --------------------------------------- */
  .row {
    display:flex; align-items:flex-start; gap:12px;
    padding:11px 0; border-bottom:1px solid var(--line);
  }
  .row-text { flex:1; min-width:0; display:flex; flex-wrap:wrap; align-items:baseline; gap:4px 10px; }
  .row-name { font:500 13.5px/1.4 var(--sans); }
  .row-sub { flex-basis:100%; color:var(--muted); font-size:12.5px; overflow-wrap:anywhere; }
  .link {
    height:auto; background:none; border:0; padding:0; cursor:pointer;
    color:var(--focus); font:400 12.5px/1.4 var(--sans);
  }
  .link:hover { text-decoration:underline; filter:none; }
  /* A switch, not a checkbox: this is on/off for a whole capability, and it
     is the control that decides whether a program runs at all. */
  .switch {
    flex:none; width:34px; height:20px; padding:0; border-radius:10px; cursor:pointer;
    border:1px solid var(--edge); background:var(--sunk); position:relative;
    transition:background .15s, border-color .15s;
  }
  .switch:hover { filter:none; border-color:var(--muted); }
  .switch::after {
    content:""; position:absolute; top:2px; left:2px; width:14px; height:14px;
    border-radius:50%; background:var(--muted); transition:transform .15s, background .15s;
  }
  .switch.on { background:var(--good); border-color:var(--good); }
  .switch.on::after { transform:translateX(14px); background:#fff; }

  .form { display:flex; flex-direction:column; gap:11px; padding:14px 0 16px; }
  .labelled { display:flex; flex-direction:column; gap:5px; }
  .small { color:var(--muted); font-size:12px; }
  .labelled input, .labelled textarea, .form select {
    width:100%; height:auto; padding:8px 10px; border:1px solid var(--edge); border-radius:8px;
    background:var(--ground); color:var(--ink); font:400 12.5px/1.5 var(--mono);
  }
  .labelled textarea { resize:vertical; min-height:72px; }
  .labelled input:disabled { color:var(--muted); background:var(--sunk); }
  .form-actions { display:flex; gap:8px; padding-top:2px; }
  .check { display:flex; align-items:center; gap:8px; font-size:12.5px; color:var(--soft); }
  .check input { flex:none; width:auto; height:auto; margin:0; }
  .tool { padding:12px 0 12px 14px; border-left:2px solid var(--line); margin:8px 0; }
  .warn { margin:4px 0 0; color:var(--bad); font-size:12.5px; max-width:52ch; }
  /* A tool's raw answer. Monospace and scrollable because it is JSON as often
     as it is prose, and wrapping it would make a one-line result three. */
  .result {
    margin:10px 0 0; padding:9px 11px; max-height:14em; overflow:auto;
    background:var(--sunk); border:1px solid var(--line); border-radius:8px;
    font:400 12px/1.5 var(--mono); white-space:pre-wrap; overflow-wrap:anywhere;
  }
  .result.is-bad { color:var(--bad); }

  /* What just happened, where it can be read from anywhere in a long pane. */
  #note {
    position:fixed; right:22px; bottom:18px; max-width:340px; margin:0;
    padding:10px 14px; border-radius:10px;
    border:1px solid var(--line); background:var(--surface); box-shadow:var(--lift);
    color:var(--soft); font-size:12.5px;
  }
  #note:empty { display:none; }
  #note.is-set { color:var(--good); border-color:var(--good); }
  #note.is-bad { color:var(--bad); border-color:var(--bad); }

  @media (prefers-reduced-motion:reduce) {
    * { transition:none !important; }
  }
  @media (max-width:760px) {
    body { grid-template-columns:1fr; grid-template-rows:auto minmax(0,1fr); }
    #nav {
      flex-direction:row; align-items:center; gap:6px; overflow-x:auto;
      border-right:0; border-bottom:1px solid var(--line); padding:10px 12px;
    }
    .brand { display:none; }
    .nav-item { width:auto; flex:none; padding:8px 12px; }
    .nav-item.on::before { top:auto; bottom:2px; left:10px; right:10px; width:auto; height:3px; }
    #main { padding:26px 20px 52px; }
    .control { flex-wrap:wrap; }
    .actions { opacity:1; pointer-events:auto; transform:none; }
  }
</style>
</head>
<body>
<nav id="nav" aria-label="Settings sections"></nav>
<main id="main"></main>
<p id="note" role="status"></p>
<script>
const nav = document.getElementById('nav');
const main = document.getElementById('main');
let state = { groups: [], fields: [], servers: [], skills: [], live: false };

// Where you are. Kept outside `draw` because every save answers with the whole
// state and redraws from it, and that must put you back where you were rather
// than at the top of the first section.
let view = { pane: 'keys', server: null, skill: null };

// The count beside each section is live state, not decoration: it is the
// answer to the question you opened settings to ask.
const PANES = [
  { id: 'keys', label: 'AI keys',
    tally: () => state.fields.filter(f => f.set).length + ' set' },
  { id: 'servers', label: 'MCP servers',
    tally: () => state.servers.filter(s => s.enabled && s.connected).length + ' live' },
  { id: 'skills', label: 'Skills',
    tally: () => state.skills.filter(s => s.enabled).length + ' on' },
];

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

function go(pane, server, skill) {
  view = {
    pane,
    server: server === undefined ? null : server,
    skill: skill === undefined ? null : skill,
  };
  draw();
  main.scrollTop = 0;
}

function head(title, about, action) {
  const h = el('header', 'pane-head');
  const t = el('div', 'pane-title');
  t.append(el('h1', null, title), el('p', null, about));
  h.append(t);
  if (action) h.append(action);
  return h;
}

function block(title, about) {
  const b = el('section', 'block');
  if (title) b.append(el('h2', null, title));
  if (about) b.append(el('p', 'block-about', about));
  return b;
}

function backTo(label, pane) {
  const b = el('button', 'back', '← ' + label);
  b.onclick = () => go(pane);
  return b;
}

function toggle(label, on, onToggle) {
  const sw = el('button', 'switch' + (on ? ' on' : ''));
  sw.setAttribute('role', 'switch');
  sw.setAttribute('aria-checked', on ? 'true' : 'false');
  sw.setAttribute('aria-label', (on ? 'Turn off ' : 'Turn on ') + label);
  sw.onclick = onToggle;
  return sw;
}

// --- AI keys -----------------------------------------------------------
//
// Still two blocks in the order a turn happens. Everything before
// transcription already runs on this machine, so hearing and answering is what
// these six values divide into.

// What the line under a box says. A field that is empty says what IRA does
// instead of it, rather than only that it is empty.
function status(f) {
  if (!f.set) return { text: f.empty, cls: '' };
  // Which store it is in, so "stored where I cannot see it" and "saved in a
  // file next to IRA" are not the same sentence.
  if (f.secret) return { text: 'Held by ' + f.store + '. Not shown again.', cls: 'is-set' };
  return { text: 'Saved.', cls: 'is-set' };
}

function field(f) {
  const row = el('div', 'field');

  const h = el('div', 'head');
  h.append(el('span', 'name', f.label), el('span', 'var', f.name));
  row.append(h);
  if (f.about) row.append(el('p', 'about', f.about));

  const control = el('div', 'control');
  const input = el('input');
  input.id = f.name;
  input.type = f.secret ? 'password' : 'text';
  input.autocomplete = 'off';
  input.spellcheck = false;
  input.value = f.secret ? '' : (f.value || '');
  input.placeholder = f.secret && f.set ? 'Stored — type to replace' : 'Not set';
  input.setAttribute('aria-describedby', 'status-' + f.name);

  const actions = el('div', 'actions');
  const save = el('button', null, f.secret ? 'Save key' : 'Save');
  save.onclick = () => send(f.name, input.value);
  actions.append(save);

  // Only offered when there is something to remove.
  if (f.set) {
    const clear = el('button', 'ghost', 'Clear');
    clear.onclick = () => { input.value = ''; send(f.name, ''); };
    actions.append(clear);
  }

  // The actions belong to the field you are working on.
  const busy = on => row.classList.toggle('busy', on);
  input.onfocus = () => busy(true);
  input.oninput = () => busy(true);
  input.onblur = () => setTimeout(() => {
    if (!row.contains(document.activeElement)) busy(false);
  }, 120);
  input.onkeydown = e => {
    if (e.key === 'Enter') send(f.name, input.value);
    if (e.key === 'Escape') { input.value = f.secret ? '' : (f.value || ''); input.blur(); }
  };

  control.append(input, actions);

  const s = status(f);
  const line = el('p', 'status ' + s.cls, s.text);
  line.id = 'status-' + f.name;
  line.setAttribute('role', 'status');

  row.append(control, line);
  return row;
}

function keysPane() {
  const pane = el('div', 'pane');
  pane.append(head('AI keys',
    'What IRA hears with and answers with. Saved here and used by the next thing she says. Nothing restarts.'));
  for (const g of state.groups) {
    const b = block(g.title, g.about);
    for (const f of state.fields.filter(f => f.group === g.id)) b.append(field(f));
    pane.append(b);
  }
  const foot = el('p', 'foot');
  foot.innerHTML =
    'Keys are held by the operating system’s keyring — Credential Manager, '
    + 'Keychain, Secret Service — and never written to a file. Everything else '
    + 'is saved in <code>ira.local.db</code> beside IRA. These are the only two '
    + 'places IRA reads from: environment variables are not consulted.';
  pane.append(foot);
  return pane;
}

// --- servers and skills ------------------------------------------------
//
// Both are lists of things that can be added, so both are a grid of cards and
// a page per card. A card is the smallest thing that answers "is this
// working"; opening one replaces the grid, because a server's form, its
// environment, its sign-in and a block per tool is far more than fits under a
// grid cell without pushing every other card down the page.

// Everything the page changes about servers and skills goes through one
// endpoint, which answers with the whole state. So every action ends the same
// way: redraw from what came back, and say what happened.
async function admin(payload, note) {
  const line = document.getElementById('note');
  line.className = '';
  line.textContent = note || 'Working…';
  try {
    const r = await fetch('/settings/admin', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    const answer = await r.json();
    if (!r.ok || answer.error) {
      line.className = 'is-bad';
      line.textContent = answer.error || ('Failed: ' + r.status + '.');
      return null;
    }
    state = answer;
    const keep = main.scrollTop;
    draw();
    main.scrollTop = keep;
    line.className = 'is-set';
    line.textContent = answer.note || 'Saved.';
    return answer;
  } catch (e) {
    line.className = 'is-bad';
    line.textContent = 'IRA is not answering.';
    return null;
  }
}

function row(title, subtitle, on, onToggle) {
  const r = el('div', 'row');
  const text = el('div', 'row-text');
  text.append(el('span', 'row-name', title), el('span', 'row-sub', subtitle));
  r.append(text, toggle(title, on, onToggle));
  return r;
}

// What this server is doing, in the order you would ask it: is it on, did it
// answer, and how much of what it offers has IRA actually been given.
function serverState(s) {
  if (!s.enabled) return 'Off. Nothing is run.';
  if (!s.connected) return 'Nothing answered.';
  const n = s.tools.length;
  if (!n) return 'Connected, no tools.';
  const offered = s.tools.filter(t => t.exposed).length;
  return n + (n === 1 ? ' tool, ' : ' tools, ') + offered + ' offered';
}

function serverCard(s) {
  const c = el('article', 'card');
  const top = el('div', 'card-top');
  const open = el('button', 'card-open', s.name);
  open.onclick = () => go('servers', s.name);
  top.append(
    el('span', 'dot' + (!s.enabled ? '' : s.connected ? ' live' : ' down')),
    open,
    toggle(s.name, s.enabled, () => admin(
      { op: 'server_toggle', name: s.name, on: !s.enabled },
      s.enabled ? 'Turning off…' : 'Connecting…')));
  c.append(top, el('p', 'card-state', serverState(s)));

  const where = s.transport === 'stdio' ? (s.command || '') : (s.url || '');
  if (where) c.append(el('p', 'card-where', where));
  c.append(el('p', 'card-foot',
    s.transport === 'stdio' ? 'A program on this machine' : 'A URL'));
  return c;
}

function serversPane() {
  if (view.server !== null) return serverDetail(view.server);

  const pane = el('div', 'pane');
  const add = el('button', null, 'Add a server');
  add.onclick = () => go('servers', '');
  pane.append(head('MCP servers',
    'Each one is a program or a URL that brings tools IRA can call. You decide which of them she may use, and which she may use without asking.',
    add));

  const grid = el('div', 'grid');
  for (const s of state.servers) grid.append(serverCard(s));
  pane.append(grid);
  if (!state.servers.length) {
    pane.append(el('p', 'empty',
      'No servers yet. Add one and IRA gains its tools the moment it answers.'));
  }
  return pane;
}

function serverDetail(name) {
  const pane = el('div', 'pane detail');
  pane.append(backTo('MCP servers', 'servers'));

  if (name === '') {
    pane.append(el('h1', null, 'Add a server'),
      el('p', 'detail-sub',
        'A program IRA runs, or a URL she calls. Its tools arrive when it connects.'));
    const b = block();
    b.append(serverForm(null));
    pane.append(b);
    return pane;
  }

  const s = state.servers.find(x => x.name === name);
  // Removed while you were looking at it. The grid is where to be.
  if (!s) { view.server = null; return serversPane(); }

  pane.append(el('h1', null, s.name), el('p', 'detail-sub', serverState(s)));

  const conn = block('Connection');
  conn.append(serverForm(s));
  pane.append(conn);

  const env = block('What it is given',
    'Values go to the keyring and are never shown again. Reconnect for a change here to take effect.');
  env.append(envBlock(s));
  pane.append(env);

  if (s.transport === 'http') {
    const sign = block('Signing in', 'Only needed if this server asks you to.');
    sign.append(signInBlock(s));
    pane.append(sign);
  }

  const tools = block('Tools',
    'Which of them IRA may use, and which she may use without asking.');
  if (s.enabled && !s.connected) {
    tools.append(el('p', 'warn',
      'Nothing answered, so there are no tools to configure. Fix the connection above and save to try again.'));
  } else if (!s.tools.length) {
    tools.append(el('p', 'row-sub', 'This server offers none.'));
  }
  for (const t of s.tools) tools.append(toolRow(s.name, t));
  pane.append(tools);
  return pane;
}

function serverForm(s) {
  const form = el('div', 'form');
  const isNew = !s;
  s = s || { name: '', transport: 'stdio', command: '', args: [], url: '', headers: {}, enabled: true };

  const name = box('Name', s.name, 'calendar');
  name.input.disabled = !isNew;   // it keys two tables; renaming is add-and-remove
  const kind = el('select');
  for (const t of ['stdio', 'http']) {
    const o = el('option', null, t === 'stdio' ? 'A program on this machine' : 'A URL');
    o.value = t;
    if (s.transport === t) o.selected = true;
    kind.append(o);
  }
  const command = box('Command', s.command || '', 'mcp-calendar');
  const args = box('Arguments', (s.args || []).join(' '), '--flag value');
  const url = box('URL', s.url || '', 'http://127.0.0.1:8765');
  const headers = box('Headers', Object.entries(s.headers || {}).map(([k, v]) => k + ': ' + v).join('\n'), 'Authorization: Bearer …');
  headers.input.replaceWith(headers.area);

  const kindRow = el('label', 'labelled');
  kindRow.append(el('span', 'small', 'Kind'), kind);

  const warning = el('p', 'warn',
    'A program here is run by IRA every time she starts. She will read it back to you and wait for a spoken yes before saving it.');

  function showKind() {
    const stdio = kind.value === 'stdio';
    command.wrap.hidden = !stdio;
    args.wrap.hidden = !stdio;
    url.wrap.hidden = stdio;
    headers.wrap.hidden = stdio;
    warning.hidden = !stdio;
  }
  kind.onchange = showKind;

  const save = el('button', null, isNew ? 'Add and connect' : 'Save and reconnect');
  save.onclick = () => admin({
    op: 'server_save',
    name: name.input.value.trim(),
    transport: kind.value,
    command: command.input.value.trim(),
    args: args.input.value.split(' ').map(a => a.trim()).filter(Boolean),
    url: url.input.value.trim(),
    headers: Object.fromEntries(headers.area.value.split('\n')
      .map(l => l.split(/:(.*)/s)).filter(p => p.length > 1 && p[0].trim())
      .map(p => [p[0].trim(), p[1].trim()])),
    enabled: s.enabled,
  }, kind.value === 'stdio' ? 'Asking you out loud…' : 'Connecting…');

  const actions = el('div', 'form-actions');
  actions.append(save);
  if (!isNew) {
    const remove = el('button', 'ghost', 'Remove');
    remove.onclick = () => {
      if (confirm('Remove ' + s.name + '? Its tools go with it.')) {
        view.server = null;
        admin({ op: 'server_delete', name: s.name }, 'Removing…');
      }
    };
    actions.append(remove);
  }

  form.append(name.wrap, kindRow, command.wrap, args.wrap, url.wrap, headers.wrap, actions, warning);
  showKind();
  return form;
}

// One tool, and what IRA believes about it. This is the confirmation gate as a
// form, so it says what each choice costs rather than only what it is called.
function toolRow(server, t) {
  const wrap = el('div', 'tool');
  const h = el('div', 'row');
  const text = el('div', 'row-text');
  text.append(el('span', 'row-name', t.name),
    el('span', 'row-sub', t.mutates === false ? 'Runs without asking.'
      : t.mutates === true ? 'Asks first — you said it changes things.'
      : 'Asks first — nobody has said whether it changes anything.'));
  h.append(text, toggle(t.name, t.exposed, () => admin({
    op: 'tool_policy', server, tool: t.name,
    exposed: !t.exposed, mutates: t.mutates, latency: t.latency, confirm: t.confirm,
  }, t.exposed ? 'Hiding…' : 'Offering…')));
  wrap.append(h);

  if (!t.exposed) return wrap;

  const form = el('div', 'form');

  const ask = el('label', 'check');
  const cb = el('input');
  cb.type = 'checkbox';
  cb.checked = t.mutates === false;
  cb.onchange = () => admin({
    op: 'tool_policy', server, tool: t.name,
    exposed: true, mutates: cb.checked ? false : null,
    latency: t.latency, confirm: t.confirm,
  }, 'Saving…');
  ask.append(cb, el('span', null, 'Read-only — do not ask before running it'));
  form.append(ask);

  if (t.mutates !== false) {
    const q = box('What she asks', t.confirm || '', 'Add that to your calendar?');
    q.input.onchange = () => admin({
      op: 'tool_policy', server, tool: t.name,
      exposed: true, mutates: t.mutates, latency: t.latency,
      confirm: q.input.value.trim(),
    }, 'Saving…');
    form.append(q.wrap);
  }

  // How long it may take, which decides whether IRA fills the silence and
  // whether the turn waits for it at all. Named by what the user hears.
  const pace = el('select');
  for (const [value, label] of [
    ['slow', 'Takes a moment — she says something while it runs'],
    ['fast', 'Instant — no filler'],
    ['background', 'Takes minutes — she answers now and reports later'],
  ]) {
    const o = el('option', null, label);
    o.value = value;
    if ((t.latency || 'slow') === value) o.selected = true;
    pace.append(o);
  }
  pace.onchange = () => admin({
    op: 'tool_policy', server, tool: t.name,
    exposed: true, mutates: t.mutates, latency: pace.value, confirm: t.confirm,
  }, 'Saving…');
  const paceRow = el('label', 'labelled');
  paceRow.append(el('span', 'small', 'How long it takes'), pace);
  form.append(paceRow);

  // Run it now, without talking to her. Finding out a tool is misconfigured
  // mid-sentence is the worst time to find out.
  const args = box('Try it with', '', '{"query": "hello"}');
  const run = el('button', 'ghost', 'Try it');
  const out = el('pre', 'result');
  out.hidden = true;
  run.onclick = async () => {
    out.hidden = false;
    out.className = 'result';
    out.textContent = 'Running…';
    try {
      const r = await fetch('/settings/admin', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ op: 'tool_try', tool: t.name, args: args.input.value }),
      });
      const answer = await r.json();
      out.className = answer.error ? 'result is-bad' : 'result';
      out.textContent = answer.error || answer.text || '(nothing)';
    } catch (e) {
      out.className = 'result is-bad';
      out.textContent = 'IRA is not answering.';
    }
  };
  const tryRow = el('div', 'form-actions');
  tryRow.append(run);
  form.append(args.wrap, tryRow, out);
  wrap.append(form);
  return wrap;
}

// What this server is given. Values go to the keyring and are never shown
// again, exactly like the provider keys.
function envBlock(s) {
  const wrap = el('div');
  if (!s.env.length) {
    wrap.append(el('p', 'row-sub', 'Nothing yet. Most servers need a token or a URL of their own.'));
  }
  for (const v of s.env) {
    const r = el('div', 'row');
    const text = el('div', 'row-text');
    text.append(el('span', 'row-name', v.name),
      el('span', 'row-sub', v.set ? 'Stored. Not shown again.' : 'No value yet — it is not passed at all.'));
    const remove = el('button', 'link', 'Remove');
    remove.onclick = () => admin({ op: 'env_delete', server: s.name, name: v.name }, 'Removing…');
    text.append(remove);
    r.append(text);
    wrap.append(r);
  }

  const form = el('div', 'form');
  const name = box('Name', '', 'GITHUB_TOKEN');
  const value = box('Value', '', 'Stored in the keyring, never in a file');
  value.input.type = 'password';
  const add = el('button', null, 'Add');
  add.onclick = () => admin({
    op: 'env_save', server: s.name,
    name: name.input.value.trim(), value: value.input.value,
  }, 'Saving…');
  const actions = el('div', 'form-actions');
  actions.append(add);
  form.append(name.wrap, value.wrap, actions);
  wrap.append(form);
  return wrap;
}

// Signing in, for a hosted server that wants OAuth rather than a header.
function signInBlock(s) {
  const wrap = el('div', 'form');
  wrap.append(el('p', 'row-sub', s.signed_in
    ? 'Signed in. The token is in the keyring and refreshes itself.'
    : 'Not signed in.'));

  const actions = el('div', 'form-actions');
  const begin = el('button', null, s.signed_in ? 'Sign in again' : 'Sign in');
  begin.onclick = async () => {
    const line = document.getElementById('note');
    line.className = '';
    line.textContent = 'Asking ' + s.name + ' how to sign in…';
    const r = await fetch('/settings/admin', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ op: 'oauth_begin', name: s.name }),
    });
    const answer = await r.json();
    if (!r.ok || answer.error) {
      line.className = 'is-bad';
      line.textContent = answer.error || 'That did not work.';
      return;
    }
    // A new tab, not this one: losing the settings page mid-sign-in would
    // leave you looking at a provider with no way back.
    line.className = '';
    line.textContent = 'Finish signing in in the tab that just opened.';
    window.open(answer.open, '_blank', 'noopener');
  };
  actions.append(begin);
  if (s.signed_in) {
    const forget = el('button', 'ghost', 'Forget');
    forget.onclick = () => admin({ op: 'oauth_forget', name: s.name }, 'Forgetting…');
    actions.append(forget);
  }
  wrap.append(actions);
  return wrap;
}

// --- skills ------------------------------------------------------------

function skillCard(s) {
  const c = el('article', 'card');
  const top = el('div', 'card-top');
  const open = el('button', 'card-open', s.name);
  open.onclick = () => go('skills', null, s.name);
  top.append(
    el('span', 'dot' + (s.enabled ? ' live' : '')),
    open,
    toggle(s.name, s.enabled, () => admin(
      { op: 'skill_toggle', name: s.name, on: !s.enabled },
      s.enabled ? 'Turning off…' : 'Turning on…')));
  c.append(top);
  c.append(el('p', 'card-desc', s.description
    || 'No description, so IRA has only the name to go on.'));
  c.append(el('p', 'card-foot path', s.path));
  return c;
}

function skillsPane() {
  if (view.skill !== null) return skillDetail(view.skill);

  const pane = el('div', 'pane');
  const add = el('button', null, 'Write a skill');
  add.onclick = () => go('skills', null, '');
  pane.append(head('Skills',
    'Instructions in your own words, for a kind of task. IRA is shown every name and description, and reads one in full when it applies.',
    add));

  const grid = el('div', 'grid');
  for (const s of state.skills) grid.append(skillCard(s));
  pane.append(grid);
  if (!state.skills.length) {
    pane.append(el('p', 'empty',
      'No skills yet. Write one for a task you explain to her more than once.'));
  }
  return pane;
}

function skillDetail(name) {
  const pane = el('div', 'pane detail');
  pane.append(backTo('Skills', 'skills'));

  if (name === '') {
    pane.append(el('h1', null, 'Write a skill'),
      el('p', 'detail-sub',
        'The description is what she reads first. Make it say when this applies.'));
    const b = block();
    b.append(skillForm(null, ''));
    pane.append(b);
    return pane;
  }

  const s = state.skills.find(x => x.name === name);
  if (!s) { view.skill = null; return skillsPane(); }

  pane.append(el('h1', null, s.name), el('p', 'detail-sub', s.enabled
    ? 'On. She reads it when it applies.'
    : 'Off. She is not shown it at all.'));

  const b = block();
  b.append(el('p', 'row-sub', 'Reading ' + s.path + '…'));
  pane.append(b);

  // The body lives in a file, so it is fetched rather than held in state. If
  // you have moved on by the time it arrives, it goes nowhere.
  fetch('/settings/admin', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ op: 'skill_body', name: s.name }),
  }).then(r => r.json()).then(answer => {
    if (view.pane === 'skills' && view.skill === name) {
      b.replaceChildren(skillForm(s, answer.text || ''));
    }
  }).catch(() => {
    b.replaceChildren(el('p', 'warn', 'Could not read ' + s.path + '.'));
  });

  return pane;
}

function skillForm(s, text) {
  const form = el('div', 'form');
  const isNew = !s;
  // The stored file carries its own front matter. The form edits the text
  // below it, so what is typed here is what IRA reads.
  const body = text.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n?/, '').trim();

  const name = box('Name', isNew ? '' : s.name, 'standup');
  name.input.disabled = !isNew;
  const description = box('When to use it', isNew ? '' : (s.description || ''),
    'How I write a standup update. Use when asked for one.');
  const instructions = box('Instructions', body, 'Three lines: yesterday, today, blockers.');
  instructions.input.replaceWith(instructions.area);
  instructions.area.rows = 12;

  const save = el('button', null, isNew ? 'Write it' : 'Save');
  save.onclick = () => admin({
    op: 'skill_save',
    name: name.input.value.trim(),
    description: description.input.value.trim(),
    body: instructions.area.value,
  }, 'Writing…');

  const actions = el('div', 'form-actions');
  actions.append(save);
  if (!isNew) {
    const remove = el('button', 'ghost', 'Delete');
    remove.onclick = () => {
      if (confirm('Delete ' + s.name + '.md?')) {
        view.skill = null;
        admin({ op: 'skill_delete', name: s.name }, 'Deleting…');
      }
    };
    actions.append(remove);
  }
  form.append(name.wrap, description.wrap, instructions.wrap, actions);
  return form;
}

// A labelled box, returned with both an input and a textarea so a caller can
// swap in whichever it wants without a second builder.
function box(label, value, placeholder) {
  const wrap = el('label', 'labelled');
  const input = el('input');
  const area = el('textarea');
  input.value = area.value = value || '';
  input.placeholder = area.placeholder = placeholder || '';
  input.autocomplete = 'off';
  input.spellcheck = area.spellcheck = false;
  wrap.append(el('span', 'small', label), input);
  return { wrap, input, area };
}

// --- drawing -----------------------------------------------------------

function draw() {
  nav.replaceChildren();
  const brand = el('div', 'brand', 'IRA');
  brand.append(el('span', null, 'Settings'));
  nav.append(brand);
  for (const p of PANES) {
    const b = el('button', 'nav-item' + (view.pane === p.id ? ' on' : ''));
    b.append(el('span', null, p.label), el('span', 'nav-count', p.tally()));
    if (view.pane === p.id) b.setAttribute('aria-current', 'page');
    b.onclick = () => go(p.id);
    nav.append(b);
  }

  main.replaceChildren(
    view.pane === 'servers' ? serversPane()
      : view.pane === 'skills' ? skillsPane()
      : keysPane());
}

async function load() {
  const r = await fetch('/settings/state');
  state = await r.json();
  const keep = main.scrollTop;
  draw();
  // The window can restore a scroll position from a previous visit, before
  // there was anything to scroll. Opening settings partway down looks like a
  // fault; a save in place should not jump you either.
  main.scrollTop = first ? 0 : keep;
  first = false;
}
let first = true;

async function send(name, value) {
  const input = document.getElementById(name);
  const line = document.getElementById('status-' + name);
  line.className = 'status';
  line.textContent = 'Saving…';
  try {
    const r = await fetch('/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, value }),
    });
    const answer = await r.json();
    if (!r.ok || answer.error) {
      line.className = 'status is-bad';
      line.textContent = answer.error || ('Not saved: ' + r.status + '.');
      return;
    }
    await load();
    // The one bit of motion: the box you just saved says so, briefly.
    const again = document.getElementById(name);
    if (again) {
      again.classList.add('saved');
      setTimeout(() => again.classList.remove('saved'), 1400);
    }
  } catch (e) {
    line.className = 'status is-bad';
    line.textContent = 'Not saved: ' + e + '.';
  }
}

load().catch(() => {
  main.replaceChildren(el('p', 'empty', 'Could not read settings. IRA may have stopped.'));
});
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

    /// The same for the settings page, which is the larger of the two and the
    /// one a stray brace in a raw string would break silently: nothing but the
    /// webview ever parses it.
    #[test]
    fn the_settings_page_is_whole() {
        assert!(SETTINGS.starts_with("<!doctype html>"));
        assert!(SETTINGS.trim_end().ends_with("</html>"));
        assert_eq!(
            SETTINGS.matches("<script>").count(),
            SETTINGS.matches("</script>").count()
        );
        // The rail's three sections, each of which draws a different pane.
        for pane in ["'keys'", "'servers'", "'skills'"] {
            assert!(SETTINGS.contains(pane), "no {pane} pane");
        }
        // Every route the page calls, so a renamed one is caught here rather
        // than by a button that quietly does nothing.
        for route in ["/settings/state", "/settings/admin", "'/settings'"] {
            assert!(SETTINGS.contains(route), "the page never calls {route}");
        }
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

    /// The admin route can make IRA spawn a process, so it is held to a higher
    /// bar than `/talk`: a request that states no origin at all is refused
    /// here and allowed there. That difference is the whole guard, and it is
    /// one word apart in the code, so it gets its own test.
    #[tokio::test]
    async fn an_admin_change_needs_a_page_that_says_it_is_the_page() {
        let req = |extra: &str| {
            std::io::Cursor::new(
                format!("Host: 127.0.0.1:8180\r\n{extra}Content-Length: 2\r\n\r\n{{}}")
                    .into_bytes(),
            )
        };
        async fn admin_ok(mut r: std::io::Cursor<Vec<u8>>) -> bool {
            let h = read_headers(&mut r, 8180).await;
            h.same_origin && h.stated_origin
        }

        assert!(admin_ok(req("Sec-Fetch-Site: same-origin\r\n")).await);
        assert!(admin_ok(req("Origin: http://127.0.0.1:8180\r\n")).await);

        assert!(!admin_ok(req("Sec-Fetch-Site: cross-site\r\n")).await);
        assert!(!admin_ok(req("Origin: https://evil.example\r\n")).await);
        // The difference from `/talk`: silence is not evidence. curl may press
        // the talk button; it may not add a server.
        assert!(!admin_ok(req("")).await);
        assert!(
            same_origin(&mut req(""), 8180).await,
            "the talk guard must stay as forgiving as it was"
        );
    }

    /// A body with a nested object must survive being read. The old reader
    /// stopped at the first closing brace, which is correct for
    /// `{name, value}` and silently truncated a server's `headers`.
    #[tokio::test]
    async fn a_nested_body_is_not_cut_at_its_first_closing_brace() {
        let body = r#"{"op":"x","headers":{"Authorization":"Bearer t"},"name":"after"}"#;
        let mut req = std::io::Cursor::new(
            format!(
                "Origin: http://127.0.0.1:8180\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        );
        let h = read_headers(&mut req, 8180).await;
        assert_eq!(h.length, body.len());
        let read = read_body(&mut req, h.length).await;
        let v: Value = serde_json::from_slice(&read).expect("a whole JSON body");
        assert_eq!(v["name"], "after", "the tail of the body was lost");
    }

    /// Spawning a command someone typed into a web page is a write, and the
    /// gate is the same one a mutating tool goes through. Nothing may be stored
    /// before the answer comes back, and no answer at all is a no.
    #[tokio::test]
    async fn a_stdio_server_is_not_saved_without_a_spoken_yes() {
        let ui = Ui::disabled();
        // No admin handle: nothing is listening to ask, which must read as a
        // refusal rather than as consent.
        assert!(!ui.ask("anything?".into()).await);

        let answer = admin_op(
            &ui,
            br#"{"op":"server_save","name":"x","transport":"stdio","command":"evil.exe"}"#,
        )
        .await;
        assert!(answer.starts_with("{\"error"), "got {answer}");

        // The shapes that are refused before anyone is even asked.
        for bad in [
            br#"{"op":"server_save","name":"../x","transport":"http","url":"http://a"}"#.as_slice(),
            br#"{"op":"server_save","name":"x","transport":"stdio"}"#.as_slice(),
            br#"{"op":"server_save","name":"x","transport":"http"}"#.as_slice(),
            br#"{"op":"server_save","name":"x","transport":"telnet","url":"http://a"}"#.as_slice(),
            br#"{"op":"nonsense"}"#.as_slice(),
        ] {
            let answer = admin_op(&ui, bad).await;
            assert!(
                answer.starts_with("{\"error"),
                "{} was accepted",
                String::from_utf8_lossy(bad)
            );
        }
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
