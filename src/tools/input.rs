use std::str::FromStr;
use std::time::{Duration, Instant};

use azalea::core::direction::Direction;
use azalea::core::game_type::GameMode;
use azalea::core::hit_result::HitResult;
use azalea::entity::LookDirection;
use azalea::entity::inventory::Inventory;
use azalea::entity::metadata::AbstractLivingUsingItem;
use azalea::interact::BlockStatePredictionHandler;
use azalea::local_player::LocalGameMode;
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::protocol::packets::game::s_player_action::{Action, ServerboundPlayerAction};
use azalea::protocol::packets::game::{ServerboundUseItem, ServerboundUseItemOn};
use azalea::registry::builtin::BlockKind;
use azalea::{BlockPos, Client, PhysicsState};
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::args::{integer, text};
use super::entities::{attack, interact};
use super::windows::swing;
use super::{Tool, alive};
use crate::calls::{Answer, Failure};

const TICK_MS: u64 = 50;

/// What the client waits between uses while the button is held, and so between uses here.
const USE_DELAY_TICKS: u32 = 4;

/// What a survival client waits after a click that hit nothing before the next one swings.
const MISS_TICKS: u32 = 10;

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Jump,
    Sneak,
    Sprint,
    Use,
    Attack,
    Hotbar,
    ScrollUp,
    ScrollDown,
    SwapOffhand,
    Drop,
}

impl Key {
    fn of(name: &str) -> Result<Key, Failure> {
        Ok(match name {
            "jump" => Key::Jump,
            "sneak" => Key::Sneak,
            "sprint" => Key::Sprint,
            "use" => Key::Use,
            "attack" => Key::Attack,
            "hotbar" => Key::Hotbar,
            "scroll-up" => Key::ScrollUp,
            "scroll-down" => Key::ScrollDown,
            "swap-offhand" => Key::SwapOffhand,
            "drop" => Key::Drop,
            _ => return Err(Failure::bad_args(format!("unknown key {name}"))),
        })
    }
}

struct Watch {
    feed: String,
    source: String,
    pattern: Regex,
}

impl Watch {
    fn of(given: &Value) -> Result<Option<Watch>, Failure> {
        if given.is_null() {
            return Ok(None);
        }
        let source = text(given, "pattern")?.to_owned();
        let pattern = Regex::new(&source)
            .map_err(|invalid| Failure::refused("BAD_PATTERN", format!("\"{source}\" is not a valid regular expression: {invalid}")))?;
        Ok(Some(Watch { feed: text(given, "feed")?.to_owned(), source, pattern }))
    }

    fn matches(&self, kind: &str, line: &str) -> bool {
        self.feed == kind && self.pattern.is_match(line)
    }

    fn describe(&self, matched: Option<&str>) -> Value {
        json!({"feed": self.feed, "pattern": self.source, "matched": matched})
    }
}

enum Phase {
    Waiting,
    Down(u32),
    Up(u32),
}

/// A key pressed tick by tick, the way the keyboard and the mouse press it.
///
/// The fabric bot presses the client's own key mappings; this one has no keyboard handler to press,
/// so it makes what that handler would have made -- the input flags the physics sends, and the use,
/// swing, attack and player action packets -- and sends them from here. A line this is waiting on is
/// matched as it is handed over from the connection, and the press goes out from the same place,
/// without the round trip through mcp-server a caller waiting on the feed would pay.
pub const PRESS_INPUT: Tool = Tool {
    name: "press-input",
    run: |bot, args| {
        Box::pin(async move {
            let key = Key::of(text(&args, "key")?)?;
            let slot = match &args["slot"] {
                Value::Null => None,
                _ => Some(integer(&args, "slot", 0)? as u8),
            };
            let hold = integer(&args, "holdTicks", 1)? as u32;
            let repeat = integer(&args, "repeat", 1)? as u32;
            let interval = integer(&args, "intervalTicks", 1)? as u32;
            let after = Watch::of(&args["after"])?;
            let until = Watch::of(&args["until"])?;
            let timeout = integer(&args, "timeoutMs", 10_000)? as u64;

            if key == Key::Hotbar && slot.is_none() {
                return Err(Failure::refused("NO_SLOT", "hotbar needs slot, 0 being the leftmost hotbar slot."));
            }
            let slot = if key == Key::Hotbar { slot } else { None };

            let client = alive(&bot, |game| game.client.clone())?;
            if client.get_component::<Inventory>().is_some_and(|inventory| inventory.container_menu.is_some()) {
                return Err(Failure::refused("WINDOW_OPEN", "a window is open, and keys go to it rather than to the game; close-window first."));
            }
            let sequence = (u64::from(repeat) * u64::from(hold) + u64::from(repeat - 1) * u64::from(interval)) * TICK_MS;
            if sequence > timeout {
                return Err(Failure::refused(
                    "TOO_LONG",
                    format!("{repeat} presses of {}, {} apart, take {sequence}ms, longer than timeoutMs ({timeout}ms).", ticks(hold), ticks(interval)),
                ));
            }

            let mut lines = bot.shown.subscribe();
            let mut clock = bot.ticks.subscribe();
            clock.borrow_and_update();

            let started = Instant::now();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
            let mut keys = Keys::new(client, key, slot.unwrap_or_default());
            /* The first press goes out on the next tick, the way the other kind of bot's does. */
            let mut phase = if after.is_some() { Phase::Waiting } else { Phase::Up(1) };
            let mut presses = 0;
            let mut waited = 0;
            let (mut after_matched, mut until_matched) = (None, None);
            let mut stopped = "done";

            loop {
                tokio::select! {
                    line = lines.recv() => {
                        let (kind, line) = match line {
                            Ok(line) => line,
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => return Err(Failure::not_in_game()),
                        };
                        match phase {
                            Phase::Waiting if after.as_ref().is_some_and(|after| after.matches(kind, &line)) => {
                                waited = started.elapsed().as_millis() as u64;
                                after_matched = Some(line);
                                keys.press();
                                presses += 1;
                                phase = Phase::Down(hold);
                            }
                            Phase::Down(_) | Phase::Up(_) if until.as_ref().is_some_and(|until| until.matches(kind, &line)) => {
                                until_matched = Some(line);
                                stopped = "until";
                                keys.release();
                                break;
                            }
                            _ => {}
                        }
                    }
                    changed = clock.changed() => {
                        if changed.is_err() {
                            return Err(Failure::not_in_game());
                        }
                        alive(&bot, |_| ())?;
                        keys.tick();
                        phase = match phase {
                            Phase::Waiting => Phase::Waiting,
                            Phase::Down(left) if left > 1 => Phase::Down(left - 1),
                            Phase::Down(_) => {
                                keys.release();
                                if presses == repeat {
                                    break;
                                }
                                Phase::Up(interval)
                            }
                            Phase::Up(left) if left > 1 => Phase::Up(left - 1),
                            Phase::Up(_) => {
                                keys.press();
                                presses += 1;
                                Phase::Down(hold)
                            }
                        };
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        if let (Phase::Waiting, Some(after)) = (&phase, &after) {
                            return Err(Failure::refused(
                                "NO_MATCH",
                                format!(
                                    "nothing on the {} feed matched /{}/ within {timeout}ms, so {} was never pressed.",
                                    after.feed,
                                    after.source,
                                    text(&args, "key")?
                                ),
                            ));
                        }
                        keys.release();
                        stopped = "timeout";
                        break;
                    }
                }
            }

            let selected = alive(&bot, |game| game.client.selected_hotbar_slot())?;
            let mut after_value = after.as_ref().map_or(Value::Null, |after| after.describe(after_matched.as_deref()));
            if !after_value.is_null() {
                after_value["waitedMs"] = json!(waited);
            }

            Ok(Answer::data(
                format!("pressed {} {presses} of {repeat} time(s), {stopped}", text(&args, "key")?),
                json!({
                    "key": text(&args, "key")?,
                    "slot": slot,
                    "presses": presses,
                    "repeat": repeat,
                    "holdTicks": hold,
                    "intervalTicks": interval,
                    "after": after_value,
                    "until": until.as_ref().map_or(Value::Null, |until| until.describe(until_matched.as_deref())),
                    "stopped": stopped,
                    "selectedSlot": selected,
                }),
            ))
        })
    },
};

/// The key being pressed, and what letting go of it undoes. Dropped with the call however it ends,
/// so a key held when a cancel or the deadline wins is not left down for as long as the bot stays.
struct Keys {
    client: Client,
    key: Key,
    slot: u8,
    down: bool,
    /// A crouch or a sprint that was on before the press, which letting go of it must not end.
    before: bool,
    held_ticks: u32,
    miss_ticks: u32,
    breaking: Option<BlockPos>,
}

impl Keys {
    fn new(client: Client, key: Key, slot: u8) -> Keys {
        Keys { client, key, slot, down: false, before: false, held_ticks: 0, miss_ticks: 0, breaking: None }
    }

    fn press(&mut self) {
        let client = &self.client;
        match self.key {
            Key::Jump => client.set_jumping(true),
            Key::Sneak => {
                self.before = client.crouching();
                client.set_crouching(true);
            }
            Key::Sprint => {
                self.before = client.query_self::<&PhysicsState, _>(|state| state.trying_to_sprint);
                client.query_self::<&mut PhysicsState, _>(|mut state| state.trying_to_sprint = true);
            }
            Key::Use => self.use_item(),
            Key::Attack => self.attack(),
            Key::Hotbar => client.set_selected_hotbar_slot(self.slot),
            Key::ScrollUp | Key::ScrollDown => {
                /* The client's own arithmetic for a wheel notch: up moves to the slot on the left, and it wraps. */
                let step = if self.key == Key::ScrollUp { 8 } else { 1 };
                client.set_selected_hotbar_slot((client.selected_hotbar_slot() + step) % 9);
            }
            Key::SwapOffhand => client.write_packet(action(Action::SwapItemWithOffhand, BlockPos::default(), 0)),
            Key::Drop => {
                let empty = client.component::<Inventory>().held_item().is_empty();
                client.write_packet(action(Action::DropItem, BlockPos::default(), 0));
                /* The client swings for a drop only when something went. */
                if !empty {
                    swing(client);
                }
            }
        }
        self.down = true;
        self.held_ticks = 0;
    }

    /// Every tick, pressed or not: the miss cooldown runs down either way, and a held use goes again.
    fn tick(&mut self) {
        self.miss_ticks = self.miss_ticks.saturating_sub(1);
        if self.down && self.key == Key::Use {
            self.held_ticks += 1;
            if self.held_ticks % USE_DELAY_TICKS == 0 && !self.using() {
                self.use_item();
            }
        }
    }

    fn release(&mut self) {
        if !std::mem::take(&mut self.down) {
            return;
        }
        let client = &self.client;
        match self.key {
            Key::Jump => client.set_jumping(false),
            Key::Sneak => client.set_crouching(self.before),
            Key::Sprint => {
                let before = self.before;
                client.query_self::<&mut PhysicsState, _>(|mut state| state.trying_to_sprint = before);
            }
            /* The client tells the server it let go only of an item that was in use: a drawn bow, food. */
            Key::Use if self.using() => client.write_packet(action(Action::ReleaseUseItem, BlockPos::default(), 0)),
            /* Nobody holds the button on a block through a bot, so a block that did not break is let go of. */
            Key::Attack => {
                if let Some(pos) = self.breaking.take()
                    && !self.creative()
                {
                    client.write_packet(action(Action::AbortDestroyBlock, pos, 0));
                }
            }
            _ => {}
        }
    }

    fn using(&self) -> bool {
        self.client.get_component::<AbstractLivingUsingItem>().is_some_and(|using| using.0)
    }

    fn creative(&self) -> bool {
        self.client.get_component::<LocalGameMode>().is_some_and(|mode| mode.current == GameMode::Creative)
    }

    /// A right-click at what the bot is looking at, with the main hand, as the client makes one.
    ///
    /// On a block the client sends the click on the block, and when the block does nothing with it,
    /// uses the item as well -- which is how a rod cast at the ground still casts. Whether the block
    /// did anything is the client's own prediction and not something azalea keeps, so the item is used
    /// unless it is a block, which the click has placed.
    fn use_item(&self) {
        let client = &self.client;
        match client.hit_result() {
            HitResult::Entity(hit) => {
                let _ = interact(client, hit.entity);
            }
            HitResult::Block(hit) => {
                if !hit.miss {
                    let seq = self.predict();
                    client.write_packet(ServerboundUseItemOn { hand: InteractionHand::MainHand, block_hit: (&hit).into(), seq });
                    let held = client.component::<Inventory>().held_item().clone();
                    if held.is_empty() || BlockKind::from_str(held.kind().to_str()).is_ok() {
                        return;
                    }
                }
                let seq = self.predict();
                let look = *client.component::<LookDirection>();
                client.write_packet(ServerboundUseItem { hand: InteractionHand::MainHand, seq, y_rot: look.y_rot(), x_rot: look.x_rot() });
            }
        }
    }

    /// A left-click: a hit on an entity, a start at breaking a block, or a swing at the air.
    fn attack(&mut self) {
        if self.miss_ticks > 0 {
            return;
        }
        let client = &self.client;
        match client.hit_result() {
            HitResult::Entity(hit) => {
                let _ = attack(client, hit.entity, hit.location);
            }
            HitResult::Block(hit) if !hit.miss => {
                let seq = self.predict();
                client.write_packet(ServerboundPlayerAction {
                    action: Action::StartDestroyBlock,
                    pos: hit.block_pos,
                    direction: hit.direction,
                    seq,
                });
                swing(client);
                self.breaking = Some(hit.block_pos);
            }
            HitResult::Block(_) => {
                swing(client);
                if !self.creative() {
                    self.miss_ticks = MISS_TICKS;
                }
            }
        }
    }

    fn predict(&self) -> u32 {
        self.client.query_self::<&mut BlockStatePredictionHandler, _>(|mut prediction| prediction.start_predicting())
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        if self.client.ecs.read().get_entity(self.client.entity).is_err() {
            return;
        }
        self.release();
    }
}

fn action(action: Action, pos: BlockPos, seq: u32) -> ServerboundPlayerAction {
    ServerboundPlayerAction { action, pos, direction: Direction::Down, seq }
}

fn ticks(count: u32) -> String {
    format!("{count} {}", if count == 1 { "tick" } else { "ticks" })
}
