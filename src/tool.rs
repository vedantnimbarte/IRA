//! Tools IRA can call, behind one trait.
//!
//! Built-ins that must be instant implement [`Tool`] directly. MCP servers
//! reach the same trait through a single adapter in P4, so the registry cannot
//! tell them apart and neither can the model.
//!
//! Three outcome shapes because tools genuinely have three shapes: a result to
//! answer with, a side effect with nothing to report, and work that takes
//! minutes and reports later. A contract of "function returning an answer" fits
//! the first two and would have to be rewritten for the third.
//!
//! Only one built-in ships here, deliberately. Memory -- the obvious first tool
//! -- is kortex-memory, which is an MCP server with sixteen tools, so it arrives
//! at P4 through the adapter rather than being reimplemented in this file.

use crate::ui::{Event, Ui};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A tool's cost, which decides whether IRA says something while it runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Latency {
    /// Fast enough that filling the silence would be worse than the silence.
    Fast,
    /// Long enough that saying nothing reads as a crash.
    Slow,
    /// Unbounded. Returns [`ToolOutcome::Started`] and reports later.
    ///
    /// Wingman is the reason this exists: a coding turn takes minutes, and no
    /// conversation can be held open that long.
    Background,
}

impl Latency {
    /// How long the tool gets before it is abandoned.
    pub fn budget(self) -> Duration {
        match self {
            Latency::Fast => Duration::from_millis(300),
            Latency::Slow => Duration::from_secs(15),
            // ponytail: ten minutes is a guess at the longest coding turn worth
            // waiting for. Raise it if a real Wingman run is ever cut off here.
            Latency::Background => Duration::from_secs(600),
        }
    }
}

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub schema: Value,
    /// Whether running this changes anything. Never taken from a server's own
    /// description -- see docs/decisions/0004.
    pub mutates: bool,
    pub latency: Latency,
    /// Spoken before a mutating tool runs. Must be answerable with yes or no.
    pub confirm: Option<String>,
}

#[derive(Debug)]
pub enum ToolOutcome {
    /// The result, in words the model can use. It is handed back to the model
    /// rather than read out verbatim: the model asked for this in order to
    /// answer something, and raw tool output is often a non-sequitur as a reply.
    Answer(String),
    /// Done; there is nothing worth reporting.
    ///
    /// The first mutating tool returns this -- a write has nothing to read back,
    /// because the confirmation already said it aloud. Only the clock ships
    /// today, and a clock has an answer.
    #[allow(dead_code)]
    Silent,
    /// Result arrives later. P8.
    #[allow(dead_code)]
    Started(u64),
}

/// What a tool may know about the turn it is running in.
///
/// Deliberately carries no TTS handle: a tool cannot speak directly, so
/// everything reaches the user through the same sentence pipeline as a reply.
pub struct ToolCtx {
    /// What the user actually said, for a tool that wants the raw phrasing
    /// rather than the model's arguments.
    #[allow(dead_code)]
    pub transcript: String,
    /// Fires on barge-in. A tool doing network work must poll this and stop
    /// before its next side effect -- cancellation is not undo, so anything it
    /// has already done stays done. Nothing built in is slow enough to need it.
    #[allow(dead_code)]
    pub cancel: CancellationToken,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome>;
}

/// A background job that has finished, on its way back to the loop.
pub struct Done {
    pub id: u64,
    pub name: String,
    /// What it produced, or why it failed.
    pub result: Result<String, String>,
}

/// Asks the main loop to get a spoken yes or no.
///
/// The loop owns the state machine and the microphone, so confirmation cannot
/// happen here. It happens there, and the answer comes back down this channel.
pub struct Confirm {
    pub question: String,
    pub reply: oneshot::Sender<bool>,
}

/// The registry, plus the confirmation path back to the loop.
pub struct Host {
    tools: HashMap<String, Arc<dyn Tool>>,
    confirm: mpsc::Sender<Confirm>,
    /// The screen sees every call and every result. The loop only hears the
    /// model's summary of them, which is the point of having a screen.
    ui: Ui,
    /// Where finished background jobs report to.
    done: mpsc::Sender<Done>,
    next_job: AtomicU64,
    /// Jobs started and not yet reported. The Host starts them, so it is the
    /// only thing that can count them.
    running: Arc<AtomicU64>,
}

impl Host {
    pub fn new(confirm: mpsc::Sender<Confirm>, ui: Ui, done: mpsc::Sender<Done>) -> Self {
        Self {
            tools: HashMap::new(),
            confirm,
            ui,
            done,
            next_job: AtomicU64::new(1),
            running: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many background jobs are still going.
    pub fn running(&self) -> u64 {
        self.running.load(Ordering::Relaxed)
    }

    pub fn add(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name.clone(), tool);
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn latency(&self, name: &str) -> Option<Latency> {
        self.tools.get(name).map(|t| t.spec().latency)
    }

    /// Runs a tool, asking first if it mutates anything, and abandoning it if it
    /// outstays its budget.
    pub async fn call(&self, name: &str, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("no such tool: {name}"))?;
        let spec = tool.spec();

        if spec.mutates {
            let question = spec
                .confirm
                .clone()
                .unwrap_or_else(|| format!("Run {name}. Yes or no?"));
            let (tx, rx) = oneshot::channel();
            self.confirm
                .send(Confirm { question, reply: tx })
                .await
                .map_err(|_| anyhow!("confirmation channel closed"))?;
            // Anything other than an explicit yes -- refusal, silence, an
            // unparsable answer, a dropped channel -- leaves the tool unrun.
            if !rx.await.unwrap_or(false) {
                return Err(anyhow!("refused"));
            }
        }

        self.ui.send(Event::Tool {
            name: name.to_string(),
            args: args.to_string(),
        });

        // Background work is detached and answered for immediately, so the loop
        // is free to take another turn while it runs.
        //
        // Its cancellation token is its own, not the turn's: the job was agreed
        // to before it started, and interrupting the sentence that asked for it
        // is not a reason to abandon work already under way.
        if spec.latency == Latency::Background {
            let id = self.next_job.fetch_add(1, Ordering::Relaxed);
            let tool = tool.clone();
            let done = self.done.clone();
            let name = name.to_string();
            let transcript = ctx.transcript.clone();
            let running = self.running.clone();
            running.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let ctx = ToolCtx { transcript, cancel: CancellationToken::new() };
                let result = match tool.call(args, &ctx).await {
                    Ok(ToolOutcome::Answer(t)) => Ok(t),
                    Ok(_) => Ok("Done.".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = done.send(Done { id, name, result }).await;
                running.fetch_sub(1, Ordering::Relaxed);
            });
            return Ok(ToolOutcome::Started(id));
        }

        let out = match tokio::time::timeout(spec.latency.budget(), tool.call(args, ctx)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("{name} outstayed its budget")),
        };

        self.ui.send(match &out {
            Ok(ToolOutcome::Answer(t)) => Event::Result {
                name: name.into(),
                ok: true,
                text: t.clone(),
            },
            Ok(_) => Event::Result { name: name.into(), ok: true, text: "done".into() },
            Err(e) => Event::Result { name: name.into(), ok: false, text: e.to_string() },
        });
        out
    }
}

/// The wall clock.
///
/// The one thing worth keeping in-process rather than behind MCP: a voice
/// assistant is asked the time constantly, the answer is three lines of code,
/// and spawning a subprocess to read a clock would cost more than the whole
/// rest of the turn.
pub struct Clock;

#[async_trait::async_trait]
impl Tool for Clock {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "clock".into(),
            description: "The current local date and time. Use this whenever the user asks \
                          what time or what day it is, or refers to today or now."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
            mutates: false,
            latency: Latency::Fast,
            confirm: None,
        }
    }

    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        // No chrono dependency for this: the model formats it for speech
        // anyway, so it only needs the parts, not a rendered string.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| anyhow!("system clock is before the epoch: {e}"))?;
        Ok(ToolOutcome::Answer(format!(
            "The Unix timestamp is {} seconds. Convert it to local time for the user.",
            now.as_secs()
        )))
    }
}

/// Cap on a tool description, which lands directly in the model's prompt.
/// Server-supplied text is an instruction channel; this bounds how much of one.
pub const DESC_MAX: usize = 1024;

const YES: &[&str] = &[
    "yes", "yeah", "yep", "yup", "sure", "ok", "okay", "do it", "go ahead", "confirm", "send it",
    "please do",
];
const NO: &[&str] = &[
    "no", "nope", "nah", "cancel", "stop", "dont", "do not", "never mind", "nevermind",
    "forget it",
];

/// Lowercased, stripped of punctuation, whitespace collapsed.
fn normalise(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a spoken answer is a yes, a no, or neither.
///
/// Matched against the whole answer rather than as substrings, so "no, don't do
/// that" does not hit "do it".
///
/// A single word said once may still arrive many times. Whisper repeats a short
/// utterance over the silence that follows it, so a perfectly clear "No." comes
/// back as "No. No. No. No. No." -- which an exact match reads as gibberish, and
/// which then fails closed for the wrong reason. With a real microphone that
/// trailing silence is guaranteed, so the repetition is the normal case rather
/// than the odd one: an answer made only of one repeated word is that word.
pub fn yes_no(transcript: &str) -> Option<bool> {
    let t = normalise(transcript);
    if t.is_empty() {
        return None;
    }
    if YES.contains(&t.as_str()) {
        return Some(true);
    }
    if NO.contains(&t.as_str()) {
        return Some(false);
    }

    // Every word the same word, whatever it was repeated.
    let mut words = t.split(' ');
    let first = words.next()?;
    if !words.all(|w| w == first) {
        return None;
    }
    if YES.contains(&first) {
        Some(true)
    } else if NO.contains(&first) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Nowhere for finished jobs to go, which is fine for tools that finish
    /// in line.
    fn jobs() -> (mpsc::Sender<Done>, mpsc::Receiver<Done>) {
        mpsc::channel(4)
    }

    fn ctx() -> ToolCtx {
        ToolCtx { transcript: String::new(), cancel: CancellationToken::new() }
    }

    /// Ambiguity must fail closed: only an explicit yes runs a mutating tool.
    #[test]
    fn only_an_explicit_yes_is_consent() {
        assert_eq!(yes_no("Yes."), Some(true));
        assert_eq!(yes_no("go ahead"), Some(true));
        assert_eq!(yes_no("No"), Some(false));
        assert_eq!(yes_no("never mind"), Some(false));

        // Whisper repeats a short answer over the silence after it. This is
        // what a real microphone produces, and it must still be an answer.
        assert_eq!(yes_no("No.
 No.
 No.
 No.
 No."), Some(false));
        assert_eq!(yes_no("Yes. Yes. Yes. Yes."), Some(true));
        assert_eq!(yes_no("go ahead"), Some(true));

        // Repetition of something that is not an answer is still not an answer.
        assert_eq!(yes_no("maybe maybe maybe"), None);
        // And a repeated word must not be read as agreement with a different one.
        assert_eq!(yes_no("yes no yes no"), None);

        // Not an answer. Must be None so the caller re-asks rather than acting.
        assert_eq!(yes_no("hmm, maybe"), None);
        assert_eq!(yes_no("what did you say"), None);
        assert_eq!(yes_no(""), None);

        // The trap a substring match would fall into: a refusal containing an
        // affirmative, and a question containing one.
        assert_eq!(yes_no("no, don't do it"), None);
        assert_eq!(yes_no("did you say yes"), None);
    }

    #[tokio::test]
    async fn the_clock_answers_without_confirmation() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut host = Host::new(tx, Ui::disabled(), jobs().0);
        host.add(Arc::new(Clock));

        let out = host.call("clock", json!({}), &ctx()).await.unwrap();
        assert!(matches!(out, ToolOutcome::Answer(_)));
        // A read-only tool must not have asked anything.
        assert!(rx.try_recv().is_err(), "a read-only tool asked for confirmation");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_error_not_a_panic() {
        let (tx, _rx) = mpsc::channel(1);
        let host = Host::new(tx, Ui::disabled(), jobs().0);
        assert!(host.call("nope", json!({}), &ctx()).await.is_err());
    }

    /// A tool slower than its declared class.
    struct Overrun;
    #[async_trait::async_trait]
    impl Tool for Overrun {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "overrun".into(),
                description: "sleeps past its budget".into(),
                schema: json!({"type": "object"}),
                mutates: false,
                latency: Latency::Fast, // 300 ms
                confirm: None,
            }
        }
        async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(ToolOutcome::Silent)
        }
    }

    /// A tool that outstays its budget is abandoned, not waited on. Otherwise a
    /// hung tool hangs the conversation.
    #[tokio::test]
    async fn a_tool_over_its_budget_is_abandoned() {
        let (tx, _rx) = mpsc::channel(1);
        let mut host = Host::new(tx, Ui::disabled(), jobs().0);
        host.add(Arc::new(Overrun));
        let err = host
            .call("overrun", json!({}), &ctx())
            .await
            .expect_err("must not wait five seconds");
        assert!(err.to_string().contains("budget"), "got {err}");
    }

    /// Work that takes longer than a conversation is willing to wait.
    struct LongJob;
    #[async_trait::async_trait]
    impl Tool for LongJob {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "build".into(),
                description: "takes a while".into(),
                schema: json!({"type": "object"}),
                mutates: false,
                latency: Latency::Background,
                confirm: None,
            }
        }
        async fn call(&self, _args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            // A job outlives the turn that asked for it, so its token must not
            // be the turn's -- interrupting the sentence is not a reason to
            // abandon work already agreed to and under way.
            assert!(!ctx.cancel.is_cancelled(), "job inherited the turn's cancellation");
            Ok(ToolOutcome::Answer("build passed".into()))
        }
    }

    /// The whole point of a background tool: the answer comes back before the
    /// work does, so the loop is free to take another turn meanwhile.
    #[tokio::test]
    async fn a_background_tool_answers_before_it_finishes() {
        let (tx, _rx) = mpsc::channel(1);
        let (done_tx, mut done_rx) = jobs();
        let mut host = Host::new(tx, Ui::disabled(), done_tx);
        host.add(Arc::new(LongJob));

        // The turn that asked is over and interrupted before the work lands.
        let turn = CancellationToken::new();
        let asked = std::time::Instant::now();
        let out = host
            .call("build", json!({}), &ToolCtx {
                transcript: "build it".into(),
                cancel: turn.clone(),
            })
            .await
            .unwrap();
        let returned = asked.elapsed();
        turn.cancel();

        let id = match out {
            ToolOutcome::Started(id) => id,
            other => panic!("expected Started, got {other:?}"),
        };
        assert!(
            returned < Duration::from_millis(200),
            "call blocked for {returned:?}; the loop would have been stuck too"
        );
        assert_eq!(host.running(), 1, "a job in flight must be countable");

        let done = done_rx.recv().await.expect("the job must report back");
        assert_eq!(done.id, id, "the report must name the job that was started");
        assert_eq!(done.name, "build");
        assert_eq!(done.result.as_deref(), Ok("build passed"));
        assert!(asked.elapsed() >= Duration::from_millis(300));
    }

    /// A tool that records whether it actually ran.
    struct Writer(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl Tool for Writer {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "writer".into(),
                description: "changes something".into(),
                schema: json!({"type": "object"}),
                mutates: true,
                latency: Latency::Fast,
                confirm: Some("Shall I do that?".into()),
            }
        }
        async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            self.0.store(true, Ordering::SeqCst);
            Ok(ToolOutcome::Silent)
        }
    }

    /// A mutating tool asks first, and a refusal leaves it unrun. This is the
    /// gate that stands between a misheard sentence and a sent email.
    #[tokio::test]
    async fn a_refused_tool_does_not_run() {
        let ran = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::channel::<Confirm>(1);
        let mut host = Host::new(tx, Ui::disabled(), jobs().0);
        host.add(Arc::new(Writer(ran.clone())));

        // Stand in for the loop: hear the question, answer no.
        tokio::spawn(async move {
            let req = rx.recv().await.expect("a mutating tool must ask");
            assert_eq!(req.question, "Shall I do that?");
            let _ = req.reply.send(false);
        });

        let err = host
            .call("writer", json!({}), &ctx())
            .await
            .expect_err("a refusal must not run the tool");
        assert!(err.to_string().contains("refused"), "got {err}");
        assert!(!ran.load(Ordering::SeqCst), "the tool ran despite a refusal");
    }

    /// The same tool runs when consent is given, so the gate is a gate and not
    /// a wall.
    #[tokio::test]
    async fn a_confirmed_tool_runs() {
        let ran = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::channel::<Confirm>(1);
        let mut host = Host::new(tx, Ui::disabled(), jobs().0);
        host.add(Arc::new(Writer(ran.clone())));

        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            let _ = req.reply.send(true);
        });

        host.call("writer", json!({}), &ctx()).await.unwrap();
        assert!(ran.load(Ordering::SeqCst), "consent was given and the tool did not run");
    }

    /// A dropped confirmation channel must read as "no", not as consent.
    #[tokio::test]
    async fn a_dropped_confirmation_is_not_consent() {
        let ran = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Confirm>(1);
        let mut host = Host::new(tx, Ui::disabled(), jobs().0);
        host.add(Arc::new(Writer(ran.clone())));

        // The loop goes away mid-question.
        drop(rx);

        assert!(host.call("writer", json!({}), &ctx()).await.is_err());
        assert!(!ran.load(Ordering::SeqCst));
    }
}
