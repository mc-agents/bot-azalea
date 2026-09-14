use std::collections::HashMap;

use azalea::FormattedText;
use azalea::chat::ChatPacket;
use serde_json::{Value, json};

use crate::bot::{Bot, now_millis};
use crate::text;

/// How often a run that stays open goes on the wire again.
const FLUSH_MS: u64 = 1_000;

/// A run nothing has repeated for this many flushes has stopped showing, whatever the server meant.
const STALE_FLUSHES: u64 = 3;

/// A chat line, pushed as it arrives. Chat is never folded: the same line twice is information.
///
/// `source` is who produced it -- a player's name, or `system` for everything else -- and not
/// where the client drew it, which is what `kind` already says. The component goes as the server
/// sent it, as far as azalea kept it, and the server does the flattening.
///
/// A system line flagged as an overlay is not chat at all: the client draws it above the hotbar,
/// and a plugin that writes its HUD that way would otherwise fill the chat feed at twenty lines a
/// second while the action bar feed stayed empty.
pub fn chat(bot: &Bot, packet: &ChatPacket) {
    if let ChatPacket::System(system) = packet
        && system.overlay
    {
        action_bar(bot, "system", &system.content);
        return;
    }

    let message = packet.message();
    let _ = bot.heard.send(message.to_string());

    if !bot.wants("chat") {
        return;
    }
    let ts = now_millis();

    bot.send(&json!({
        "t": "event",
        "seq": bot.next_seq(),
        "kind": "chat",
        "source": packet.sender().unwrap_or_else(|| "system".into()),
        "text": message.to_string(),
        "component": text::component(&message),
        "ts": ts,
        "firstTs": ts,
        "repeats": 1,
        "closed": false,
    }));
}

pub fn action_bar(bot: &Bot, source: &str, message: &FormattedText) {
    drawn(bot, "actionBar", source, message);
}

/// A title or a subtitle, told apart by `source` on the one feed.
pub fn title(bot: &Bot, source: &str, message: &FormattedText) {
    drawn(bot, "title", source, message);
}

/// The dialog itself rides in `data`, as the game serialises one; the title is the text, so a
/// server that reads nothing else still has something to show.
pub fn dialog(bot: &Bot, dialog: Value) {
    let title = dialog
        .get("title")
        .cloned()
        .and_then(|title| serde_json::from_value::<FormattedText>(title).ok())
        .map(|title| title.to_string())
        .unwrap_or_default();
    let component = dialog.get("title").cloned().unwrap_or(Value::Null);

    fold(bot, Line { kind: "dialog", source: "dialog", text: title, segments: None, component, data: Some(dialog) });
}

pub fn dialog_closed(bot: &Bot) {
    let said = "the dialog was closed";
    fold(bot, Line { kind: "dialog", source: "closed", text: said.into(), segments: None, component: json!(said), data: None });
}

/// Only the feeds a server draws with stacked glyphs carry segments. Chat is prose, and splitting
/// it at every style change turns one sentence into a dozen fragments.
fn drawn(bot: &Bot, kind: &'static str, source: &str, message: &FormattedText) {
    fold(bot, Line {
        kind,
        source,
        text: message.to_string(),
        segments: Some(text::segments(message)),
        component: text::component(message),
        data: None,
    });
}

struct Line<'a> {
    kind: &'static str,
    source: &'a str,
    text: String,
    segments: Option<Value>,
    component: Value,
    data: Option<Value>,
}

pub struct Run {
    seq: u64,
    kind: &'static str,
    source: String,
    text: String,
    segments: Option<Value>,
    component: Value,
    data: Option<Value>,
    first_ts: u64,
    ts: u64,
    repeats: u64,
    sent_at: u64,
}

/// The run each folded feed has open, by feed.
pub type Runs = HashMap<&'static str, Run>;

/// An action bar redrawn twenty times a second is one thing showing, not twenty things happening.
///
/// The run keeps its sequence number and every re-send reuses it, so mcp-server updates that line
/// in place rather than waking a waiter on each frame -- a wait would otherwise match a line that
/// was already up before it was called. What is re-sent once a second is what keeps "how long has
/// this been showing" current without paying for the redraws.
fn fold(bot: &Bot, line: Line) {
    if !bot.wants(line.kind) {
        return;
    }
    let now = now_millis();
    let mut frames = Vec::new();

    {
        let mut runs = bot.runs.borrow_mut();

        if let Some(run) = runs.get_mut(line.kind)
            && run.source == line.source
            && run.text == line.text
            && run.component == line.component
            && run.data == line.data
        {
            run.repeats += 1;
            run.ts = now;
            if now - run.sent_at >= FLUSH_MS {
                run.sent_at = now;
                frames.push(frame(run, false));
            }
        } else {
            if let Some(ended) = runs.remove(line.kind) {
                frames.push(frame(&ended, true));
            }
            let run = Run {
                seq: bot.next_seq(),
                kind: line.kind,
                source: line.source.to_owned(),
                text: line.text,
                segments: line.segments,
                component: line.component,
                data: line.data,
                first_ts: now,
                ts: now,
                repeats: 1,
                sent_at: now,
            };
            frames.push(frame(&run, false));
            runs.insert(line.kind, run);
        }
    }

    for frame in frames {
        bot.send(&frame);
    }
}

/// Re-send the runs that are due, and close the ones nothing has repeated in a while.
pub fn flush(bot: &Bot) {
    let now = now_millis();
    let mut frames = Vec::new();

    bot.runs.borrow_mut().retain(|_, run| {
        if now - run.ts >= FLUSH_MS * STALE_FLUSHES {
            frames.push(frame(run, true));
            return false;
        }
        if now - run.sent_at >= FLUSH_MS {
            run.sent_at = now;
            frames.push(frame(run, false));
        }
        true
    });

    for frame in frames {
        bot.send(&frame);
    }
}

/// The bot left the world, so nothing it was showing is showing any more.
pub fn close_all(bot: &Bot) {
    let now = now_millis();
    let ended: Vec<Value> = bot
        .runs
        .borrow_mut()
        .drain()
        .map(|(_, mut run)| {
            run.ts = now;
            frame(&run, true)
        })
        .collect();

    for frame in ended {
        bot.send(&frame);
    }
}

fn frame(run: &Run, closed: bool) -> Value {
    let mut event = json!({
        "t": "event",
        "seq": run.seq,
        "kind": run.kind,
        "source": run.source,
        "text": run.text,
        "component": run.component,
        "ts": run.ts,
        "firstTs": run.first_ts,
        "repeats": run.repeats,
        "closed": closed,
    });
    if let Some(segments) = &run.segments {
        event["segments"] = segments.clone();
    }
    if let Some(data) = &run.data {
        event["data"] = data.clone();
    }
    event
}
