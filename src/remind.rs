//! Reminders and timers: "remind me in twenty minutes to take the bread out",
//! "set a timer for ten minutes", "remind me at 3pm to call Sam".
//!
//! A timer is a reminder with nothing to add, so there is one mechanism. Due
//! times are stored in the database as Unix milliseconds, so a reminder set
//! before IRA closes -- or the machine restarts -- still goes off afterwards.
//! One that fell due while she was not running is said as soon as she is,
//! and says that it is late.
//!
//! Delivery goes through the same queue as a finished background job and
//! `POST /say`: news from outside the conversation waits for the floor rather
//! than interrupting a sentence, and a pip marks its arrival.

use crate::tool::{Latency, Tool, ToolCtx, ToolOutcome, ToolSpec};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Longest a reminder may say. It is read aloud.
const TEXT_MAX: usize = 300;
/// Furthest ahead a reminder may be set.
const HORIZON_SECS: i64 = 366 * 86_400;
/// Later than this and it is said as late rather than as on time.
const LATE_MS: i64 = 2 * 60_000;

/// Pokes the scheduler when a reminder is added or cancelled, so it does not
/// sleep through one set for thirty seconds from now.
fn wake() -> &'static Notify {
    static WAKE: OnceLock<Notify> = OnceLock::new();
    WAKE.get_or_init(Notify::new)
}

// --------------------------------------------------------------- local time --

/// Seconds to add to UTC for local time, right now.
#[cfg(windows)]
pub fn local_offset_secs() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz = TIME_ZONE_INFORMATION::default();
    // SAFETY: a zeroed, correctly sized struct for the call to fill in.
    let which = unsafe { GetTimeZoneInformation(&mut tz) };
    let bias = match which {
        2 => tz.Bias + tz.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
        1 => tz.Bias + tz.StandardBias, // TIME_ZONE_ID_STANDARD
        _ => tz.Bias,
    };
    -(bias as i64) * 60
}

#[cfg(unix)]
// `tm_gmtoff` is a `c_long`: already `i64` on 64-bit targets, not on 32-bit.
#[allow(clippy::useless_conversion)]
pub fn local_offset_secs() -> i64 {
    let now = crate::transcript::now() as libc::time_t;
    // SAFETY: `localtime_r` only writes into the struct it is given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        i64::from(tm.tm_gmtoff)
    }
}

#[cfg(not(any(windows, unix)))]
pub fn local_offset_secs() -> i64 {
    0
}

/// (year, month, day, hour, minute, weekday with Monday = 0) for a count of
/// seconds already shifted into local time. Howard Hinnant's civil-from-days,
/// because this is the only calendar arithmetic IRA does.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, usize) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    // 1970-01-01 was a Thursday.
    let weekday = (days + 3).rem_euclid(7) as usize;
    (year, month, day, (sod / 3600) as u32, (sod % 3600 / 60) as u32, weekday)
}

const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

/// "Monday 14 September 2026, 15:42" for a Unix-milliseconds moment, in local
/// time.
pub fn describe_local(unix_ms: i64) -> String {
    let (y, m, d, h, min, wd) = civil(unix_ms.div_euclid(1000) + local_offset_secs());
    format!("{} {d} {} {y}, {h:02}:{min:02}", WEEKDAYS[wd], MONTHS[m as usize - 1])
}

/// Local midnight at the start of today, as Unix milliseconds.
pub fn start_of_today_ms() -> i64 {
    let offset = local_offset_secs();
    let local = crate::audit::now_ms().div_euclid(1000) + offset;
    (local - local.rem_euclid(86_400) - offset) * 1000
}

/// "in 2 hours and 5 minutes", for a duration a voice will say.
fn in_words(ms: i64) -> String {
    let mins = (ms + 30_000) / 60_000;
    let plural = |n: i64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if ms < 60_000 {
        return format!("in {}", plural((ms / 1000).max(1), "second"));
    }
    let (days, hours, m) = (mins / 1440, mins % 1440 / 60, mins % 60);
    let parts: Vec<String> = [(days, "day"), (hours, "hour"), (m, "minute")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, u)| plural(n, u))
        .collect();
    format!("in {}", parts.join(" and "))
}

/// When a reminder is due, from the model's arguments: `in_seconds`, or `at`
/// as a local "HH:MM" plus an optional `day_offset` (1 is tomorrow). Without a
/// day offset, `at` is its next occurrence.
///
/// ponytail: `at` uses today's UTC offset, so a reminder set across a
/// daylight-saving change lands an hour off. Per-date offsets fix that if it
/// ever matters.
fn due_from(args: &Value, now_ms: i64, offset_secs: i64) -> Result<i64> {
    if let Some(secs) = args["in_seconds"].as_i64() {
        if !(1..=HORIZON_SECS).contains(&secs) {
            bail!("a reminder has to be between a second and a year away");
        }
        return Ok(now_ms + secs * 1000);
    }
    let at = args["at"].as_str().ok_or_else(|| anyhow!("say when: in_seconds, or at as HH:MM"))?;
    let (h, m) = at
        .trim()
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)))
        .filter(|(h, m)| (0..24).contains(h) && (0..60).contains(m))
        .ok_or_else(|| anyhow!("\"{at}\" is not a time like 15:30"))?;
    let local_now = now_ms.div_euclid(1000) + offset_secs;
    let midnight = local_now - local_now.rem_euclid(86_400);
    let mut local = midnight + h * 3600 + m * 60;
    match args["day_offset"].as_i64() {
        Some(days) if (0..=366).contains(&days) => local += days * 86_400,
        Some(_) => bail!("day_offset has to be between 0 and 366"),
        None if local <= local_now => local += 86_400,
        None => {}
    }
    if local <= local_now {
        bail!("that time has already passed");
    }
    Ok((local - offset_secs) * 1000)
}

// ------------------------------------------------------------------- tools --

/// Sets a reminder or a timer.
pub struct Remind;

#[async_trait::async_trait]
impl Tool for Remind {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remind".into(),
            description: "Sets a reminder or a timer that IRA says aloud when it is due, even if \
                          she is restarted in between. Give `text` -- what to say, e.g. \"Take the \
                          bread out\" or, for a timer, \"Your ten minute timer is done\" -- and \
                          either `in_seconds`, or `at` as a 24-hour local time \"HH:MM\" with an \
                          optional `day_offset` (1 for tomorrow). Without `day_offset`, `at` means \
                          the next time the clock shows it."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "in_seconds": { "type": "integer", "minimum": 1 },
                    "at": { "type": "string", "description": "HH:MM, 24-hour, local time" },
                    "day_offset": { "type": "integer", "minimum": 0 },
                },
                "required": ["text"],
            }),
            mutates: false,
            latency: Latency::Fast,
            confirm: None,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let text = args["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            bail!("a reminder needs something to say");
        }
        if text.chars().count() > TEXT_MAX {
            bail!("keep it under {TEXT_MAX} characters -- it is read aloud");
        }
        let now = crate::audit::now_ms();
        let due = due_from(&args, now, local_offset_secs())?;
        crate::db::reminder_add(due, text)?;
        wake().notify_one();
        Ok(ToolOutcome::Answer(format!(
            "Reminder set for {}, {}: \"{text}\".",
            describe_local(due),
            in_words(due - now)
        )))
    }
}

/// Lists or cancels reminders.
pub struct Reminders;

#[async_trait::async_trait]
impl Tool for Reminders {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "reminders".into(),
            description: "Lists the reminders and timers that are set (`action`: \"list\"), or \
                          cancels one (`action`: \"cancel\") -- by its `id` from the list, or by a \
                          word from its `text`. With only one set, cancel needs neither."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "cancel"] },
                    "id": { "type": "integer" },
                    "text": { "type": "string" },
                },
                "required": ["action"],
            }),
            mutates: false,
            latency: Latency::Fast,
            confirm: None,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let all = crate::db::reminders()?;
        let now = crate::audit::now_ms();
        let line = |r: &crate::db::ReminderRow| {
            format!("#{} {}, {}: {}", r.id, describe_local(r.due), in_words((r.due - now).max(0)), r.text)
        };
        match args["action"].as_str().unwrap_or("list") {
            "list" => Ok(ToolOutcome::Answer(if all.is_empty() {
                "No reminders or timers are set.".into()
            } else {
                all.iter().map(line).collect::<Vec<_>>().join("\n")
            })),
            "cancel" => {
                let want = args["text"].as_str().map(str::to_lowercase);
                let found: Vec<_> = match (args["id"].as_i64(), want) {
                    (Some(id), _) => all.iter().filter(|r| r.id == id).collect(),
                    (None, Some(w)) => all.iter().filter(|r| r.text.to_lowercase().contains(&w)).collect(),
                    (None, None) => all.iter().collect(),
                };
                match found.as_slice() {
                    [] => Ok(ToolOutcome::Answer("No reminder matches that.".into())),
                    [one] => {
                        crate::db::reminder_delete(one.id)?;
                        wake().notify_one();
                        Ok(ToolOutcome::Answer(format!("Cancelled: {}", one.text)))
                    }
                    many => Ok(ToolOutcome::Answer(format!(
                        "More than one matches -- which?\n{}",
                        many.iter().map(|r| line(r)).collect::<Vec<_>>().join("\n")
                    ))),
                }
            }
            other => bail!("unknown action: {other}"),
        }
    }
}

// --------------------------------------------------------------- scheduler --

/// What is said when a reminder goes off.
fn announcement(text: &str, late: bool) -> String {
    if late {
        format!("While I was away, you asked me to remind you: {text}")
    } else {
        format!("Reminder: {text}")
    }
}

/// Says reminders as they fall due. Checks at least once a minute, and sooner
/// when the next one is closer or one is added.
///
/// At least once, not exactly once: the reminder is deleted after it is
/// queued, so a crash between the two says it again on the next start rather
/// than never.
pub fn start(say: mpsc::Sender<(String, bool)>) {
    tokio::spawn(async move {
        loop {
            let pending = crate::db::reminders().unwrap_or_else(|e| {
                tracing::error!(?e, "reminders unreadable");
                Vec::new()
            });
            let now = crate::audit::now_ms();
            for r in pending.iter().filter(|r| r.due <= now) {
                let late = now - r.due > LATE_MS;
                if say.send((announcement(&r.text, late), true)).await.is_err() {
                    return;
                }
                if let Err(e) = crate::db::reminder_delete(r.id) {
                    tracing::error!(?e, id = r.id, "reminder said but not removed");
                }
                let result = if late { "said late" } else { "said" };
                crate::audit::record("reminder", "reminder", &r.text, Some(result), Some(true));
            }
            let wait = pending
                .iter()
                .filter(|r| r.due > now)
                .map(|r| r.due - now)
                .min()
                .unwrap_or(60_000)
                .clamp(50, 60_000);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(wait as u64)) => {}
                _ = wake().notified() => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_right_either_side_of_the_epoch_and_a_leap_day() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 3));
        // 2024-02-29 12:34 UTC, a Thursday.
        assert_eq!(civil(1_709_210_040), (2024, 2, 29, 12, 34, 3));
        // 1969-12-31 23:59, a Wednesday.
        assert_eq!(civil(-60), (1969, 12, 31, 23, 59, 2));
    }

    #[test]
    fn at_means_the_next_time_the_clock_shows_it() {
        // 2026-09-14 10:00 local, at UTC+5:30.
        let offset = 5 * 3600 + 1800;
        let now = (1_789_380_000 - offset) * 1000;
        assert_eq!(civil(now / 1000 + offset).3, 10);

        let later = due_from(&json!({"at": "15:30"}), now, offset).unwrap();
        assert_eq!((later - now) / 60_000, 5 * 60 + 30);

        // 09:00 has passed today, so it is tomorrow.
        let tomorrow = due_from(&json!({"at": "09:00"}), now, offset).unwrap();
        assert_eq!((tomorrow - now) / 60_000, 23 * 60);

        let explicit = due_from(&json!({"at": "15:30", "day_offset": 1}), now, offset).unwrap();
        assert_eq!(explicit - later, 86_400_000);

        assert!(due_from(&json!({"at": "25:00"}), now, offset).is_err());
        assert!(due_from(&json!({"at": "09:00", "day_offset": 0}), now, offset).is_err());
        assert!(due_from(&json!({}), now, offset).is_err());
        assert_eq!(due_from(&json!({"in_seconds": 90}), now, offset).unwrap(), now + 90_000);
    }

    #[test]
    fn durations_read_as_speech() {
        assert_eq!(in_words(20_000), "in 20 seconds");
        assert_eq!(in_words(60_000), "in 1 minute");
        assert_eq!(in_words(125 * 60_000), "in 2 hours and 5 minutes");
        assert_eq!(in_words(26 * 3_600_000), "in 1 day and 2 hours");
    }

    #[test]
    fn a_late_reminder_says_it_is_late() {
        assert_eq!(announcement("call Sam", false), "Reminder: call Sam");
        assert!(announcement("call Sam", true).starts_with("While I was away"));
    }

    /// Set, list, cancel by a word, and the scheduler's view of what is due --
    /// against a real database in a temp directory.
    #[test]
    fn reminders_round_trip_through_the_database() {
        let _guard = crate::db::cwd_lock();
        let dir = std::env::temp_dir().join("ira-remind-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("IRA_DATA", &dir);
        let _ = std::fs::remove_file(crate::db::path());

        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let ctx = ToolCtx { transcript: String::new(), cancel: tokio_util::sync::CancellationToken::new() };
        let answer = |out: Result<ToolOutcome>| match out.unwrap() {
            ToolOutcome::Answer(t) => t,
            _ => panic!("expected an answer"),
        };

        let set = answer(rt.block_on(Remind.call(json!({"text":"take the bread out","in_seconds":600}), &ctx)));
        assert!(set.contains("in 10 minutes"), "{set}");
        rt.block_on(Remind.call(json!({"text":"call Sam","in_seconds":7200}), &ctx)).unwrap();

        let list = answer(rt.block_on(Reminders.call(json!({"action":"list"}), &ctx)));
        assert!(list.find("bread").unwrap() < list.find("Sam").unwrap(), "soonest first: {list}");

        let ambiguous = answer(rt.block_on(Reminders.call(json!({"action":"cancel"}), &ctx)));
        assert!(ambiguous.starts_with("More than one"), "{ambiguous}");
        let cancelled = answer(rt.block_on(Reminders.call(json!({"action":"cancel","text":"BREAD"}), &ctx)));
        assert_eq!(cancelled, "Cancelled: take the bread out");
        assert_eq!(crate::db::reminders().unwrap().len(), 1);

        std::env::remove_var("IRA_DATA");
    }
}
