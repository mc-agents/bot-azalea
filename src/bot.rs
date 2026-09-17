use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use azalea::FormattedText;
use azalea::core::entity_id::MinecraftEntityId;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::config::Config;
use crate::feeds::Runs;
use crate::game::{Departing, Game};
use crate::link::frame;
use crate::text::Line;

pub const KIND: &str = "azalea";
pub const MINECRAFT_VERSION: &str = "26.1.2";

/// Everything the link, the calls and the game share. The whole bot runs on one thread, so this is
/// cells rather than locks: nothing here is ever touched from two places at once.
pub struct Bot {
    pub config: Config,
    outbox: RefCell<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    linked: Cell<bool>,
    /// Calls in flight by id. The sender is taken out when a cancel arrives; the entry stays until the
    /// call answers, because its absence is how a call learns the link dropped under it.
    pub calls: RefCell<HashMap<String, Option<oneshot::Sender<String>>>>,
    pub game: RefCell<Option<Game>>,
    /// The connection the last leave ended, until the next join has waited for it to go.
    pub departing: RefCell<Option<Departing>>,
    feeds: RefCell<Map<String, Value>>,
    /// How often a folded run goes on the wire again, as the server's handshake set it.
    repeat_flush_ms: Cell<u64>,
    seq: Cell<u64>,
    /// The folded feeds' open runs, which outlive a connection only long enough to be closed.
    pub runs: RefCell<Runs>,
    /// Every chat line as text, for a call waiting on the one thing a proxy only says in chat.
    pub heard: broadcast::Sender<String>,
    /// The action bar, the titles and the sounds in the wordings a pattern is matched against, by
    /// feed, for a press that has to land on the tick a line arrives rather than a round trip later.
    pub shown: broadcast::Sender<(&'static str, Line)>,
    /// The chat lines still on screen, newest first, kept as components so a click event in one can
    /// be pressed. Bounded like the client's own history: what is off the screen is not clickable for
    /// a player either.
    pub said: RefCell<VecDeque<FormattedText>>,
    pub ticks: watch::Sender<u64>,
    /// The window the server last sent the whole of, or `None` once a new one has opened and its
    /// contents have not arrived. Sent on every arrival, equal or not, because an arrival is what a
    /// click waits for.
    pub contents: watch::Sender<Option<i32>>,
    /// The window the server last opened and its title, from the packet rather than the menu, so a
    /// click that the server answered with a new window can say which.
    pub opened: RefCell<Option<(i32, FormattedText)>>,
    /// How many windows the server has opened, so a click can tell a window redrawn under the id
    /// it clicked from the one it clicked.
    pub opens: Cell<u64>,
    /// The window the server last closed.
    pub closed: watch::Sender<Option<i32>>,
    /// The entities the server said were hurt, by network id, for a swing waiting to hear that its
    /// target was. Every one is kept rather than the last, because the pump hands over a frame's
    /// packets before the swing gets to look, and another entity's hurt can come behind the target's.
    pub hurt: broadcast::Sender<MinecraftEntityId>,
    /// What the last status said that a move can change, and the tick it went out on.
    pub reported: RefCell<Option<(String, u64)>>,
}

impl Bot {
    pub fn new(config: Config) -> Bot {
        Bot {
            config,
            outbox: RefCell::new(None),
            linked: Cell::new(false),
            calls: RefCell::new(HashMap::new()),
            game: RefCell::new(None),
            departing: RefCell::new(None),
            feeds: RefCell::new(Map::new()),
            repeat_flush_ms: Cell::new(1_000),
            seq: Cell::new(0),
            runs: RefCell::new(Runs::new()),
            heard: broadcast::channel(64).0,
            shown: broadcast::channel(256).0,
            said: RefCell::new(VecDeque::new()),
            ticks: watch::channel(0).0,
            contents: watch::channel(None).0,
            opened: RefCell::new(None),
            opens: Cell::new(0),
            closed: watch::channel(None).0,
            hurt: broadcast::channel(64).0,
            reported: RefCell::new(None),
        }
    }

    /// A hundred lines is more than a client shows and less than anything worth measuring.
    pub fn remember(&self, line: FormattedText) {
        const KEPT: usize = 100;
        let mut said = self.said.borrow_mut();
        said.push_front(line);
        said.truncate(KEPT);
    }

    pub fn attach(&self, outbox: mpsc::UnboundedSender<Vec<u8>>) {
        *self.outbox.borrow_mut() = Some(outbox);
    }

    /// The link is gone. Calls in flight cannot be answered on a link that no longer exists, so
    /// they are dropped rather than left waiting for one that will not come back with their ids.
    pub fn detach(&self) {
        *self.outbox.borrow_mut() = None;
        self.linked.set(false);
        self.calls.borrow_mut().clear();
    }

    pub fn accepted(&self, feeds: Map<String, Value>, repeat_flush_ms: Option<u64>) {
        *self.feeds.borrow_mut() = feeds;
        if let Some(every) = repeat_flush_ms.filter(|every| *every > 0) {
            self.repeat_flush_ms.set(every);
        }
        self.linked.set(true);
    }

    /// Whether the server accepted our hello. What /readyz answers, because the operator's LINK
    /// column is that probe and nothing else outside the protocol knows.
    pub fn linked(&self) -> bool {
        self.linked.get()
    }

    pub fn send(&self, message: &Value) {
        if let Some(outbox) = self.outbox.borrow().as_ref() {
            let _ = outbox.send(frame::json(message));
        }
    }

    pub fn repeat_flush_ms(&self) -> u64 {
        self.repeat_flush_ms.get()
    }

    pub fn wants(&self, feed: &str) -> bool {
        self.feeds.borrow().get(feed).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.set(self.seq.get() + 1);
        self.seq.get()
    }

    pub fn busy(&self) -> usize {
        self.calls.borrow().len()
    }

    /// A status carries whatever the bot knows at the time. The fields that describe a world are
    /// only there while it is in one.
    pub fn status(&self, state: &str, reason: Option<&str>) {
        let mut status = json!({
            "t": "status",
            "state": state,
            "ts": now_millis(),
            "mcVersion": MINECRAFT_VERSION,
        });
        if let Some(game) = self.game.borrow().as_ref() {
            game.describe(&mut status);
        }
        if let Some(reason) = reason {
            status["reason"] = json!(reason);
        }
        self.send(&status);
    }

    pub fn log(&self, level: &str, message: impl Into<String>) {
        self.send(&json!({"t": "log", "level": level, "message": message.into()}));
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}
