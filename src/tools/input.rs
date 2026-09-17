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
use azalea::mining::{MiningQueued, StopMiningBlockEvent};
use azalea::movement::LastSentInput;
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::protocol::packets::game::s_player_action::{Action, ServerboundPlayerAction};
use azalea::protocol::packets::game::{ServerboundUseItem, ServerboundUseItemOn};
use azalea::registry::builtin::BlockKind;
use azalea::{BlockPos, Client, PhysicsState};
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::args::{integer, text};
use super::entities::{aimed, attack, interact};
use super::windows::swing;
use super::{Tool, alive, tick};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};
use crate::text::Line;

pub(super) const TICK_MS: u64 = 50;

/// What the client waits between uses while the button is held, and so between uses here.
const USE_DELAY_TICKS: u32 = 4;

/// What a survival client waits after a click that hit nothing before the next one swings.
const MISS_TICKS: u32 = 10;

/// How long a block the button was let go of can still be on its way to starting to break, and the
/// most a released key is waited on to reach the server.
const SETTLE_TICKS: u32 = 3;

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

    fn name(self) -> &'static str {
        match self {
            Key::Jump => "jump",
            Key::Sneak => "sneak",
            Key::Sprint => "sprint",
            Key::Use => "use",
            Key::Attack => "attack",
            Key::Hotbar => "hotbar",
            Key::ScrollUp => "scroll-up",
            Key::ScrollDown => "scroll-down",
            Key::SwapOffhand => "swap-offhand",
            Key::Drop => "drop",
        }
    }
}

/// A line on one of the shown feeds that a press waits on, or stops at.
pub(super) struct Watch {
    pub(super) feed: String,
    pub(super) source: String,
    pattern: Regex,
}

impl Watch {
    fn of(given: &Value) -> Result<Option<Watch>, Failure> {
        if given.is_null() {
            return Ok(None);
        }
        Watch::parse(given).map(Some)
    }

    pub(super) fn parse(given: &Value) -> Result<Watch, Failure> {
        let source = text(given, "pattern")?.to_owned();
        let pattern = Regex::new(&source).map_err(|invalid| {
            Failure::refused(
                "BAD_PATTERN",
                format!("\"{source}\" is not a valid regular expression: {invalid}"),
            )
        })?;
        Ok(Watch {
            feed: text(given, "feed")?.to_owned(),
            source,
            pattern,
        })
    }

    pub(super) fn matches(&self, kind: &str, line: &Line) -> bool {
        self.feed == kind && line.matches(&self.pattern)
    }

    pub(super) fn describe(&self, matched: Option<&str>) -> Value {
        json!({"feed": self.feed, "pattern": self.source, "matched": matched})
    }
}

/// press-input's arguments as the wire gives them, checked.
pub(super) struct PressArgs {
    key: Key,
    pub(super) slot: Option<u8>,
    pub(super) hold: u32,
    repeat: u32,
    interval: u32,
    after: Option<Watch>,
    until: Option<Watch>,
    timeout: u64,
}

impl PressArgs {
    pub(super) fn parse(args: &Value) -> Result<PressArgs, Failure> {
        let key = Key::of(text(args, "key")?)?;
        let slot = match &args["slot"] {
            Value::Null => None,
            _ => Some(integer(args, "slot", 0)? as u8),
        };
        if key == Key::Hotbar && slot.is_none() {
            return Err(Failure::refused(
                "NO_SLOT",
                "hotbar needs slot, 0 being the leftmost hotbar slot.",
            ));
        }
        Ok(PressArgs {
            key,
            slot: if key == Key::Hotbar { slot } else { None },
            hold: integer(args, "holdTicks", 1)? as u32,
            repeat: integer(args, "repeat", 1)? as u32,
            interval: integer(args, "intervalTicks", 1)? as u32,
            after: Watch::of(&args["after"])?,
            until: Watch::of(&args["until"])?,
            timeout: integer(args, "timeoutMs", 10_000)? as u64,
        })
    }

    pub(super) fn key(&self) -> &'static str {
        self.key.name()
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
            let args = PressArgs::parse(&args)?;
            game_takes_keys(&bot)?;
            let (repeat, hold, interval, timeout) = (args.repeat, args.hold, args.interval, args.timeout);
            let sequence =
                (u64::from(repeat) * u64::from(hold) + u64::from(repeat - 1) * u64::from(interval)) * TICK_MS;
            if sequence > timeout {
                return Err(Failure::refused(
                    "TOO_LONG",
                    format!(
                        "{repeat} presses of {}, {} apart, take {sequence}ms, longer than timeoutMs ({timeout}ms).",
                        ticks(hold),
                        ticks(interval)
                    ),
                ));
            }

            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
            let key = args.key();
            let data = press(&bot, args, Some(deadline)).await?;
            let summary = format!(
                "pressed {key} {} of {repeat} time(s), {}",
                data["presses"],
                data["stopped"].as_str().unwrap_or_default()
            );
            Ok(Answer::data(summary, data))
        })
    },
};

/// Whether a key would reach the game: a window takes the keys while one is open.
pub(super) fn game_takes_keys(bot: &Bot) -> Result<(), Failure> {
    let client = alive(bot, |game| game.client.clone())?;
    /* A dialog is a screen on the other kind of bot, and keys go to it there too. */
    let dialog = alive(bot, |game| game.hud.borrow().dialog.is_some())?;
    if dialog
        || client
            .get_component::<Inventory>()
            .is_some_and(|inventory| inventory.container_menu.is_some())
    {
        return Err(Failure::refused(
            "WINDOW_OPEN",
            "a window is open, and keys go to it rather than to the game; close-window first.",
        ));
    }
    Ok(())
}

/// The presses, made and answered as press-input's DTO. The first press goes out on the tick after
/// the call, and a key let go of is settled with the server before the answer. With no deadline
/// the presses run until they are done, or until whatever awaits them drops them.
pub(super) async fn press(
    bot: &Bot,
    args: PressArgs,
    deadline: Option<tokio::time::Instant>,
) -> Result<Value, Failure> {
    let PressArgs {
        key,
        slot,
        hold,
        repeat,
        interval,
        after,
        until,
        timeout,
    } = args;
    let client = alive(bot, |game| game.client.clone())?;

    let mut lines = bot.shown.subscribe();
    let mut clock = bot.ticks.subscribe();
    clock.borrow_and_update();

    let started = Instant::now();
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
                        after_matched = Some(line.shown);
                        keys.press();
                        presses += 1;
                        phase = Phase::Down(hold);
                    }
                    Phase::Down(_) | Phase::Up(_) if until.as_ref().is_some_and(|until| until.matches(kind, &line)) => {
                        until_matched = Some(line.shown);
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
                alive(bot, |_| ())?;
                keys.tick();
                phase = match phase {
                    Phase::Waiting => Phase::Waiting,
                    /* A tick counts once the server has been told, however long azalea takes to tell it. */
                    Phase::Down(left) if !keys.arrived() => Phase::Down(left),
                    Phase::Up(left) if !keys.arrived() => Phase::Up(left),
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
            () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                if let (Phase::Waiting, Some(after)) = (&phase, &after) {
                    return Err(Failure::refused(
                        "NO_MATCH",
                        format!(
                            "nothing on the {} feed matched /{}/ within {timeout}ms, so {} was never pressed.",
                            after.feed,
                            after.source,
                            key.name()
                        ),
                    ));
                }
                keys.release();
                stopped = "timeout";
                break;
            }
        }
    }

    /*
    A break that was on its way when the button came up is caught on the ticks after, and a
    key let go of is told to the server before the answer, so a press in the next call is a
    press of its own and not lost in the same input.
    */
    for _ in 0..SETTLE_TICKS {
        if key != Key::Attack && keys.arrived() {
            break;
        }
        tick(bot).await;
        keys.tick();
    }
    let selected = alive(bot, |game| game.client.selected_hotbar_slot())?;
    let mut after_value = after
        .as_ref()
        .map_or(Value::Null, |after| after.describe(after_matched.as_deref()));
    if !after_value.is_null() {
        after_value["waitedMs"] = json!(waited);
    }

    Ok(json!({
        "key": key.name(),
        "slot": slot,
        "presses": presses,
        "repeat": repeat,
        "holdTicks": hold,
        "intervalTicks": interval,
        "after": after_value,
        "until": until.as_ref().map_or(Value::Null, |until| until.describe(until_matched.as_deref())),
        "stopped": stopped,
        "selectedSlot": selected,
    }))
}

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
}

impl Keys {
    fn new(client: Client, key: Key, slot: u8) -> Keys {
        Keys {
            client,
            key,
            slot,
            down: false,
            before: false,
            held_ticks: 0,
            miss_ticks: 0,
        }
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
        if !self.down && self.key == Key::Attack {
            self.stop_mining();
        }
        if self.down && self.key == Key::Use {
            self.held_ticks += 1;
            if self.held_ticks.is_multiple_of(USE_DELAY_TICKS) && !self.using() {
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
            /*
            The client tells the server it let go of an item it had started using: a drawn bow,
            food. It knows from its own prediction; this bot only has the flag the server syncs a
            tick later, which a tap lets go of before it arrives, and a release the server has
            nothing in use for is a no-op there, whereas one withheld leaves the bow drawn. So
            every use is let go of, as use-held-item lets go of its hold.
            */
            Key::Use => client.write_packet(action(Action::ReleaseUseItem, BlockPos::default(), 0)),
            /* Let go before a block broke, the client tells the server it stopped. */
            Key::Attack => {
                client.left_click_mine(false);
                self.stop_mining();
            }
            _ => {}
        }
    }

    /// Whether the input the server was last sent agrees with the key.
    ///
    /// azalea sends the movement flags on its own tick and the bot runs on another task, so a key
    /// held for one tick here can go down and up again between two of azalea's and never reach the
    /// server: a jump that turns a conversation's page turned nothing. The other keys are packets
    /// sent the moment they are pressed.
    fn arrived(&self) -> bool {
        if !matches!(self.key, Key::Jump | Key::Sneak | Key::Sprint) {
            return true;
        }
        let wanted = self.down || self.before;
        /* Nothing sent yet is every key up. */
        let sent = self
            .client
            .get_component::<LastSentInput>()
            .map(|sent| sent.0.clone())
            .unwrap_or_default();
        match self.key {
            Key::Jump => sent.jump == self.down,
            Key::Sneak => sent.shift == wanted,
            _ => sent.sprint == wanted,
        }
    }

    /// Give up on the block being broken, if any.
    ///
    /// A block the button was held on starts breaking a tick or two after the button went down: the
    /// left-click mining asks, the next update queues it and the tick after that starts. A button let
    /// go of in between finds nothing to stop, and what it started then breaks the block on its own.
    /// So the start still waiting is taken away, and this runs again on the ticks after a release.
    fn stop_mining(&self) {
        let client = &self.client;
        client.ecs.write().entity_mut(client.entity).remove::<MiningQueued>();
        if client.is_mining() {
            client
                .ecs
                .write()
                .write_message(StopMiningBlockEvent { entity: client.entity });
        }
    }

    fn using(&self) -> bool {
        self.client
            .get_component::<AbstractLivingUsingItem>()
            .is_some_and(|using| using.0)
    }

    fn creative(&self) -> bool {
        self.client
            .get_component::<LocalGameMode>()
            .is_some_and(|mode| mode.current == GameMode::Creative)
    }

    /// A right-click at what the bot is looking at, with the main hand, as the client makes one.
    ///
    /// On a block the client asks the block first and the item after. A block that does something
    /// with the click -- a chest opening, a door swinging, a button going down -- takes it, and the
    /// item is not used; a block that does nothing passes it on, and the item's own use on a block
    /// comes next, which for a block item is placing it. Only a click nothing took uses the item in
    /// the air as well, which is how a rod cast at the ground still casts. Crouching with something
    /// in hand skips the block, the way it lets a player place against a chest.
    ///
    /// The game decides that on the client from its own block and item code, which azalea does not
    /// have, so the blocks a click opens or toggles whatever is in hand are named here, and an item
    /// is taken to be placed when a block goes by its name.
    fn use_item(&self) {
        let client = &self.client;
        /* The entity pick first: azalea's own misses the interaction hitbox a model is clicked through. */
        if let Some((entity, at)) = aimed(client) {
            let _ = interact(client, entity, Some(at));
            return;
        }
        match client.hit_result() {
            HitResult::Entity(hit) => {
                let _ = interact(client, hit.entity, None);
            }
            HitResult::Block(hit) => {
                if !hit.miss {
                    let seq = self.predict();
                    client.write_packet(ServerboundUseItemOn {
                        hand: InteractionHand::MainHand,
                        block_hit: (&hit).into(),
                        seq,
                    });

                    let inventory = client.component::<Inventory>();
                    let held = inventory.held_item().clone();
                    let has_items = !held.is_empty() || !inventory.inventory_menu.as_player().offhand.is_empty();
                    drop(inventory);
                    let clicked = client
                        .world()
                        .read()
                        .get_block_state(hit.block_pos)
                        .map(BlockKind::from);

                    let block_took_it = !(client.crouching() && has_items) && clicked.is_some_and(interactive);
                    if block_took_it || held.is_empty() || BlockKind::from_str(held.kind().to_str()).is_ok() {
                        swing(client);
                        return;
                    }
                }
                let seq = self.predict();
                let look = *client.component::<LookDirection>();
                client.write_packet(ServerboundUseItem {
                    hand: InteractionHand::MainHand,
                    seq,
                    y_rot: look.y_rot(),
                    x_rot: look.x_rot(),
                });
            }
        }
    }

    /// A left-click: a hit on an entity, or a swing at the air with a survival client's miss cooldown.
    ///
    /// A block is not hit here. Holding the button is what breaks one, and azalea's own left-click
    /// mining is that hold: it starts on the block looked at, sends the progress a survival client
    /// sends, moves on to the next block and stops on a miss, for as long as it stays on.
    fn attack(&mut self) {
        let client = &self.client;
        /* A hitbox azalea's pick passes through is hit, not the block behind it mined. */
        let hitbox = aimed(client);
        client.left_click_mine(hitbox.is_none());
        if self.miss_ticks > 0 {
            return;
        }
        if let Some((entity, _)) = hitbox {
            let _ = attack(client, entity, None);
            return;
        }
        match client.hit_result() {
            HitResult::Entity(hit) => {
                let _ = attack(client, hit.entity, Some(hit.location));
            }
            HitResult::Block(hit) if !hit.miss => {}
            HitResult::Block(_) => {
                swing(client);
                if !self.creative() {
                    self.miss_ticks = MISS_TICKS;
                }
            }
        }
    }

    fn predict(&self) -> u32 {
        self.client
            .query_self::<&mut BlockStatePredictionHandler, _>(|mut prediction| prediction.start_predicting())
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

/// A block that takes a right-click whatever is in hand: it opens, toggles, rings or is sat in, and
/// the client uses nothing after it. Iron doors and trapdoors open only by redstone and pass it on.
fn interactive(block: BlockKind) -> bool {
    const TAKEN_BY: &[&str] = &[
        "chest",
        "barrel",
        "shulker_box",
        "furnace",
        "smoker",
        "hopper",
        "dispenser",
        "dropper",
        "crafter",
        "brewing_stand",
        "crafting_table",
        "anvil",
        "enchanting_table",
        "grindstone",
        "loom",
        "stonecutter",
        "cartography_table",
        "smithing_table",
        "lectern",
        "beacon",
        "door",
        "trapdoor",
        "fence_gate",
        "button",
        "lever",
        "_bed",
        "bell",
        "repeater",
        "comparator",
        "daylight_detector",
        "note_block",
        "cake",
        "jukebox",
        "respawn_anchor",
        "command_block",
        "sign",
        "structure_block",
        "jigsaw",
    ];
    let name = block.to_str();
    !name.starts_with("minecraft:iron_") && TAKEN_BY.iter().any(|part| name.contains(part))
}

fn action(action: Action, pos: BlockPos, seq: u32) -> ServerboundPlayerAction {
    ServerboundPlayerAction {
        action,
        pos,
        direction: Direction::Down,
        seq,
    }
}

fn ticks(count: u32) -> String {
    format!("{count} {}", if count == 1 { "tick" } else { "ticks" })
}
