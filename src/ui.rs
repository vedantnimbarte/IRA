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
}

/// The settings page's reach into the running process.
///
/// A `Weak` on purpose: the registry holds a `Ui` so it can report every call
/// to the screen, and a strong handle back would be a cycle that leaks the
/// registry for the life of the process.
pub struct Admin {
    pub host: std::sync::Weak<crate::tool::Host>,
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
            json!({
                "name": s.name,
                "transport": s.transport,
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
    crate::db::server_delete(name)
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
/// Laid out as the two stages of a turn that leave this machine, in the order
/// they happen, rather than as six equal rows: everything before transcription
/// already runs here, so hearing and answering is what these values actually
/// divide into. The thread down the left is the only ornament, and it is
/// carrying that order.
///
/// Values are set in a monospace face and nothing else is, because a key or a
/// URL is read character by character and that is a legibility need rather than
/// a label style. No web fonts: IRA runs without a network and a settings page
/// that fetched a typeface would be the only part of her that did not.
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
    --ground:#eef1f5; --surface:#fff; --sunk:#f5f7fa;
    --ink:#131820; --soft:#3f4956; --muted:#78849a; --faint:#9aa5b8;
    --line:#dfe4ec; --edge:#cdd5e2;
    --good:#0d7a68; --bad:#a72f3c; --focus:#2a6df4;
    /* The orb's own light, sampled from it. Used once, on the thread. */
    --thread:linear-gradient(180deg,#19a2fe,#7f9bfb,#cc9bfd,#fe84e4,#71fbf0);
    --sans:"Segoe UI Variable Text","Segoe UI Variable","Segoe UI",system-ui,sans-serif;
    --display:"Segoe UI Variable Display","Segoe UI Variable","Segoe UI",system-ui,sans-serif;
    --mono:"Cascadia Mono",Consolas,ui-monospace,monospace;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --ground:#0c1017; --surface:#141a23; --sunk:#10151d;
      --ink:#e8ecf2; --soft:#b6c0cf; --muted:#7d8899; --faint:#5f6a7a;
      --line:#222a35; --edge:#2d3745;
      --good:#4fd6c4; --bad:#f08a94; --focus:#6ba2ff;
    }
  }
  * { box-sizing:border-box; }
  html { background:var(--ground); }
  body {
    margin:0; background:var(--ground); color:var(--ink);
    font:400 14px/1.55 var(--sans);
    -webkit-font-smoothing:antialiased;
  }
  .page { max-width:600px; margin:0 auto; padding:36px 40px 44px; }

  /* --- the head ------------------------------------------------------- */
  h1 {
    font:300 26px/1.2 var(--display); letter-spacing:-.015em;
    margin:0 0 6px;
  }
  .lede { margin:0; color:var(--muted); font-size:13.5px; max-width:46ch; }

  /* --- a stage -------------------------------------------------------- */
  .stage { position:relative; padding:30px 0 0 24px; }
  /* The thread: the one ornament, and it is carrying the order of a turn. */
  .stage::before {
    content:""; position:absolute; left:0; top:36px; bottom:12px; width:2px;
    border-radius:2px; background:var(--thread); opacity:.85;
  }
  .stage:last-of-type::before { bottom:20px; }
  h2 { font:400 17px/1.3 var(--display); letter-spacing:-.01em; margin:0 0 4px; }
  .stage > p { margin:0; color:var(--muted); font-size:13px; max-width:52ch; }

  /* --- a field -------------------------------------------------------- */
  .field { padding:18px 0 16px; border-bottom:1px solid var(--line); }
  .field:first-of-type { padding-top:20px; }
  .field:last-child { border-bottom:0; padding-bottom:2px; }
  .head { display:flex; align-items:baseline; gap:10px; flex-wrap:wrap; }
  .name { font-size:14px; font-weight:600; color:var(--ink); }
  /* The variable name, because this is what `doctor` and the docs call it. */
  .var { font:400 11.5px/1 var(--mono); color:var(--faint); }
  .about { margin:2px 0 0; color:var(--muted); font-size:12.5px; line-height:1.45; max-width:54ch; }

  .control { display:flex; gap:8px; margin-top:10px; align-items:stretch; }
  input {
    flex:1 1 auto; min-width:0; height:38px; padding:0 12px;
    border:1px solid var(--edge); border-radius:9px;
    background:var(--sunk); color:var(--ink);
    font:400 13px/1 var(--mono);
    transition:border-color .15s ease, background .15s ease;
  }
  input::placeholder { color:var(--faint); font-family:var(--sans); font-size:13px; }
  input:hover { border-color:var(--muted); }
  input:focus { outline:none; border-color:var(--focus); background:var(--surface); }
  input.saved { border-color:var(--good); }

  /* Actions arrive when you are working on a field, so six of them are not
     competing for attention while you read. */
  /* A reserved column, so every box is the same width whether or not its
     field has something to clear. */
  .actions {
    display:flex; gap:8px; flex:0 0 152px; opacity:0; pointer-events:none;
    transform:translateX(-4px);
    transition:opacity .16s ease, transform .16s ease;
  }
  .field.busy .actions { opacity:1; pointer-events:auto; transform:none; }
  button {
    height:38px; padding:0 15px; border-radius:9px; cursor:pointer;
    font:600 13px/1 var(--sans);
    border:1px solid var(--ink); background:var(--ink); color:var(--ground);
    transition:filter .12s ease;
  }
  button.ghost { background:transparent; color:var(--muted); border-color:var(--edge); font-weight:400; }
  button:hover { filter:brightness(1.12); }
  button.ghost:hover { color:var(--ink); border-color:var(--muted); }
  button:focus-visible, input:focus-visible { outline:2px solid var(--focus); outline-offset:2px; }

  .status { margin:7px 0 0; font-size:12.5px; color:var(--muted); min-height:1.3em; }
  .status.is-set { color:var(--good); }
  .status.is-bad { color:var(--bad); }

  footer {
    margin-top:30px; padding-top:18px; border-top:1px solid var(--line);
    color:var(--faint); font-size:12.5px; line-height:1.5; max-width:56ch;
  }
  footer code { font:400 12px/1 var(--mono); color:var(--muted); }

  /* --- rows, forms and switches: the servers and skills stages -------- */
  .row {
    display:flex; align-items:flex-start; gap:12px;
    padding:11px 0; border-bottom:1px solid var(--line);
  }
  .row-text { flex:1; min-width:0; display:flex; flex-wrap:wrap; align-items:baseline; gap:4px 10px; }
  .row-name { font:500 13.5px/1.4 var(--sans); }
  .row-sub {
    flex-basis:100%; color:var(--muted); font-size:12.5px;
    overflow-wrap:anywhere;
  }
  .link {
    background:none; border:0; padding:0; cursor:pointer;
    color:var(--focus); font:400 12.5px/1.4 var(--sans);
  }
  .link:hover { text-decoration:underline; }
  /* A switch, not a checkbox: this is on/off for a whole capability, and it
     is the control that decides whether a program runs at all. */
  .switch {
    flex:none; width:34px; height:20px; border-radius:10px; cursor:pointer;
    border:1px solid var(--edge); background:var(--sunk); position:relative;
    transition:background .15s, border-color .15s;
  }
  .switch::after {
    content:""; position:absolute; top:2px; left:2px; width:14px; height:14px;
    border-radius:50%; background:var(--muted); transition:transform .15s, background .15s;
  }
  .switch.on { background:var(--good); border-color:var(--good); }
  .switch.on::after { transform:translateX(14px); background:#fff; }
  .switch:focus-visible { outline:2px solid var(--focus); outline-offset:2px; }

  .panel { padding:4px 0 14px; border-bottom:1px solid var(--line); }
  .form { display:flex; flex-direction:column; gap:10px; padding:12px 0; }
  .labelled { display:flex; flex-direction:column; gap:4px; }
  .small { color:var(--muted); font-size:12px; }
  .labelled input, .labelled textarea, .form select {
    width:100%; padding:7px 9px; border:1px solid var(--edge); border-radius:6px;
    background:var(--surface); color:var(--ink); font:400 12.5px/1.5 var(--mono);
  }
  .labelled textarea { resize:vertical; min-height:64px; }
  .labelled input:disabled { color:var(--muted); background:var(--sunk); }
  .form-actions { display:flex; gap:8px; }
  .check { display:flex; align-items:center; gap:7px; font-size:12.5px; color:var(--soft); }
  .check input { margin:0; }
  .tool { padding:8px 0 8px 14px; border-left:2px solid var(--line); margin:6px 0; }
  .warn { margin:2px 0 0; color:var(--bad); font-size:12.5px; max-width:52ch; }
  .note { margin:14px 0 0; min-height:1.4em; color:var(--muted); font-size:12.5px; }
  .note.is-set { color:var(--good); }
  .note.is-bad { color:var(--bad); }

  @media (prefers-reduced-motion:reduce) {
    * { transition:none !important; }
  }
  @media (max-width:520px) {
    .page { padding:36px 22px 48px; }
    .control { flex-wrap:wrap; }
    .actions { opacity:1; pointer-events:auto; transform:none; }
  }
</style>
</head>
<body>
<div class="page">
  <h1>Settings</h1>
  <p class="lede">Saved here and used by the next thing IRA says. Nothing restarts.</p>
  <div id="stages"></div>
  <p class="note" id="note" role="status"></p>
  <footer id="foot"></footer>
</div>
<script>
const stages = document.getElementById('stages');
let state = { groups: [], fields: [], servers: [], skills: [], live: false };

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

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

  const head = el('div', 'head');
  head.append(el('span', 'name', f.label), el('span', 'var', f.name));
  row.append(head);
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

// --- servers and skills ------------------------------------------------
//
// Both stages are lists of things that can be added, so both are: a row per
// item, a form that opens on the one you are editing, and one button that
// opens an empty form. No modal, no route change -- the window is small and
// a dialog inside a webview inside an overlay is a lot of layers for a form
// with four boxes.

// Everything the page changes about servers and skills goes through one
// endpoint, which answers with the whole state. So every action ends the same
// way: redraw from what came back, and say what happened.
async function admin(payload, note) {
  const line = document.getElementById('note');
  line.className = 'note';
  line.textContent = note || 'Working…';
  try {
    const r = await fetch('/settings/admin', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    const answer = await r.json();
    if (!r.ok || answer.error) {
      line.className = 'note is-bad';
      line.textContent = answer.error || ('Failed: ' + r.status + '.');
      return null;
    }
    state = answer;
    const keep = window.scrollY;
    draw();
    window.scrollTo(0, keep);
    const after = document.getElementById('note');
    after.className = 'note is-set';
    after.textContent = answer.note || 'Saved.';
    return answer;
  } catch (e) {
    line.className = 'note is-bad';
    line.textContent = 'IRA is not answering.';
    return null;
  }
}

function row(title, subtitle, on, onToggle) {
  const r = el('div', 'row');
  const text = el('div', 'row-text');
  text.append(el('span', 'row-name', title), el('span', 'row-sub', subtitle));
  const sw = el('button', 'switch' + (on ? ' on' : ''));
  sw.setAttribute('role', 'switch');
  sw.setAttribute('aria-checked', on ? 'true' : 'false');
  sw.setAttribute('aria-label', (on ? 'Turn off ' : 'Turn on ') + title);
  sw.onclick = onToggle;
  r.append(text, sw);
  return r;
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

  function showKind() {
    const stdio = kind.value === 'stdio';
    command.wrap.hidden = !stdio;
    args.wrap.hidden = !stdio;
    url.wrap.hidden = stdio;
    headers.wrap.hidden = stdio;
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
        admin({ op: 'server_delete', name: s.name }, 'Removing…');
      }
    };
    actions.append(remove);
  }

  form.append(name.wrap, kindRow, command.wrap, args.wrap, url.wrap, headers.wrap, actions);
  showKind();
  if (kind.value === 'stdio') {
    form.append(el('p', 'warn', 'A program here is run by IRA every time she starts. She will read it back to you and wait for a spoken yes before saving it.'));
  }
  return form;
}

// One tool, and what IRA believes about it. This is the confirmation gate as a
// form, so it says what each choice costs rather than only what it is called.
function toolRow(server, t) {
  const wrap = el('div', 'tool');
  const head = el('div', 'row');
  const text = el('div', 'row-text');
  text.append(el('span', 'row-name', t.name),
    el('span', 'row-sub', t.mutates === false ? 'Runs without asking.'
      : t.mutates === true ? 'Asks first — you said it changes things.'
      : 'Asks first — nobody has said whether it changes anything.'));
  const sw = el('button', 'switch' + (t.exposed ? ' on' : ''));
  sw.setAttribute('role', 'switch');
  sw.setAttribute('aria-checked', t.exposed ? 'true' : 'false');
  sw.setAttribute('aria-label', (t.exposed ? 'Hide ' : 'Offer ') + t.name);
  sw.onclick = () => admin({
    op: 'tool_policy', server, tool: t.name,
    exposed: !t.exposed, mutates: t.mutates, latency: t.latency, confirm: t.confirm,
  }, t.exposed ? 'Hiding…' : 'Offering…');
  head.append(text, sw);
  wrap.append(head);

  if (!t.exposed) return wrap;

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
  wrap.append(ask);

  if (t.mutates !== false) {
    const q = box('What she asks', t.confirm || '', 'Add that to your calendar?');
    q.input.onchange = () => admin({
      op: 'tool_policy', server, tool: t.name,
      exposed: true, mutates: t.mutates, latency: t.latency,
      confirm: q.input.value.trim(),
    }, 'Saving…');
    wrap.append(q.wrap);
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
  wrap.append(paceRow);
  return wrap;
}

function serversStage() {
  const stage = el('section', 'stage');
  stage.append(el('h2', null, 'Tools'),
    el('p', null, 'MCP servers. Each one brings tools IRA can call; you decide which of them she may use, and which she may use without asking.'));

  for (const s of state.servers) {
    const where = s.transport === 'stdio' ? (s.command || '') : (s.url || '');
    const status = !s.enabled ? 'Off. ' + where
      : s.connected ? s.tools.length + (s.tools.length === 1 ? ' tool. ' : ' tools. ') + where
      : 'Not connected. ' + where;
    const r = row(s.name, status, s.enabled,
      () => admin({ op: 'server_toggle', name: s.name, on: !s.enabled },
        s.enabled ? 'Turning off…' : 'Connecting…'));

    const open = el('button', 'link', 'Edit');
    const panel = el('div', 'panel');
    panel.hidden = true;
    open.onclick = () => { panel.hidden = !panel.hidden; };
    r.querySelector('.row-text').append(open);

    panel.append(serverForm(s));
    for (const t of s.tools) panel.append(toolRow(s.name, t));
    if (s.enabled && !s.connected) {
      panel.append(el('p', 'warn', 'Nothing answered, so there are no tools to configure. Fix it above and save to try again.'));
    }
    stage.append(r, panel);
  }

  const add = el('button', 'link', '+ Add a server');
  const adding = el('div', 'panel');
  adding.hidden = true;
  add.onclick = () => {
    adding.hidden = !adding.hidden;
    if (!adding.hidden) { adding.replaceChildren(serverForm(null)); }
  };
  stage.append(add, adding);
  return stage;
}

function skillsStage() {
  const stage = el('section', 'stage');
  stage.append(el('h2', null, 'Skills'),
    el('p', null, 'Instructions in your own words, for a kind of task. IRA is shown the names and descriptions, and reads one in full when it applies.'));

  for (const s of state.skills) {
    const r = row(s.name, s.description || s.path, s.enabled,
      () => admin({ op: 'skill_toggle', name: s.name, on: !s.enabled },
        s.enabled ? 'Turning off…' : 'Turning on…'));
    const open = el('button', 'link', 'Edit');
    const panel = el('div', 'panel');
    panel.hidden = true;
    open.onclick = async () => {
      panel.hidden = !panel.hidden;
      if (panel.hidden) return;
      panel.replaceChildren(el('p', 'row-sub', 'Reading ' + s.path + '…'));
      const r2 = await fetch('/settings/admin', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ op: 'skill_body', name: s.name }),
      });
      const answer = await r2.json();
      panel.replaceChildren(skillForm(s, answer.text || ''));
    };
    r.querySelector('.row-text').append(open);
    stage.append(r, panel);
  }

  const add = el('button', 'link', '+ Write a skill');
  const adding = el('div', 'panel');
  adding.hidden = true;
  add.onclick = () => {
    adding.hidden = !adding.hidden;
    if (!adding.hidden) adding.replaceChildren(skillForm(null, ''));
  };
  stage.append(add, adding);
  return stage;
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
  instructions.area.rows = 10;

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
      if (confirm('Delete ' + s.name + '.md?')) admin({ op: 'skill_delete', name: s.name }, 'Deleting…');
    };
    actions.append(remove);
  }
  form.append(name.wrap, description.wrap, instructions.wrap, actions);
  if (!isNew) form.append(el('p', 'row-sub', s.path));
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

function draw() {
  stages.replaceChildren();
  for (const g of state.groups) {
    const stage = el('section', 'stage');
    stage.append(el('h2', null, g.title), el('p', null, g.about));
    for (const f of state.fields.filter(f => f.group === g.id)) stage.append(field(f));
    stages.append(stage);
  }
  stages.append(serversStage(), skillsStage());
  document.getElementById('foot').innerHTML =
    'Keys are held by the operating system’s keyring — Credential Manager, '
    + 'Keychain, Secret Service — and never written to a file. Everything else '
    + 'is saved in <code>ira.local.db</code> beside IRA. These are the only two '
    + 'places IRA reads from: environment variables are not consulted.';
}

async function load() {
  const r = await fetch('/settings/state');
  state = await r.json();
  const keepScroll = window.scrollY;
  draw();
  // The window can restore a scroll position from a previous visit, before
  // there was anything to scroll. Opening settings at the bottom of the page
  // looks like a fault; a save in place should not jump you either.
  window.scrollTo(0, first ? 0 : keepScroll);
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
  stages.append(el('p', 'status is-bad', 'Could not read settings. IRA may have stopped.'));
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
