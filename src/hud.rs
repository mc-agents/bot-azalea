//! What a vanilla client keeps from the packets azalea decodes and throws away.
//!
//! azalea's handlers for the action bar, titles, boss bars, the scoreboard, the clock and dialogs
//! are empty, so nothing of them is left to read by the time a tool asks. They are read here
//! instead: a system inside the ECS picks those packets out of everything the connection receives
//! and hands only them to the bot, which keeps the state a tool reads and pushes the feeds.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use azalea::app::{App, Plugin, Update};
use azalea::brigadier::suggestion::Suggestions;
use azalea::core::data_registry::ResolvableDataRegistry;
use azalea::ecs::prelude::*;
use azalea::packet::game::ReceiveGamePacketEvent;
use azalea::protocol::packets::game::ClientboundGamePacket;
use azalea::protocol::packets::game::c_commands::ClientboundCommands;
use azalea::protocol::packets::game::c_boss_event::{BossBarColor, BossBarOverlay, Operation};
use azalea::protocol::packets::game::c_game_event::EventType;
use azalea::protocol::packets::game::c_set_display_objective::DisplaySlot;
use azalea::protocol::packets::game::c_set_objective::Method;
use azalea::registry::data::DimensionKind;
use azalea::registry::{DataRegistry, Holder};
use azalea::{FormattedText, Identifier};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::bot::Bot;
use crate::dialog::Open;
use crate::feeds;

/// Where the packets the bot keeps go, one per connection.
#[derive(Component)]
pub struct HudPackets(pub mpsc::UnboundedSender<Arc<ClientboundGamePacket>>);

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, forward);
    }
}

/// The packets azalea already decoded, filtered where they were received.
///
/// Not azalea's packet event, which sends every packet over the event channel and wakes the bot's
/// task for each. A connection receives hundreds a second -- chunks, and every entity in range
/// moving -- and a bot that is one of fifty processes on a node has no use for all but a handful.
/// Matching the variant here is a comparison; what crosses the channel is an Arc of the few that
/// matter, which the connection had allocated anyway.
fn forward(mut received: MessageReader<ReceiveGamePacketEvent>, listeners: Query<&HudPackets>) {
    for ReceiveGamePacketEvent { entity, packet } in received.read() {
        if kept(packet)
            && let Ok(listener) = listeners.get(*entity)
        {
            let _ = listener.0.send(packet.clone());
        }
    }
}

fn kept(packet: &ClientboundGamePacket) -> bool {
    use ClientboundGamePacket as P;
    matches!(
        packet,
        P::SetActionBarText(_)
            | P::SetTitleText(_)
            | P::SetSubtitleText(_)
            | P::BossEvent(_)
            | P::SetObjective(_)
            | P::SetDisplayObjective(_)
            | P::SetScore(_)
            | P::ResetScore(_)
            | P::SetTime(_)
            | P::GameEvent(_)
            | P::ShowDialog(_)
            | P::ClearDialog(_)
            /* The command tree, which says whether the client would run a command a dialog asks for. */
            | P::Commands(_)
            | P::PlayerInfoUpdate(_)
            | P::PlayerInfoRemove(_)
            | P::CommandSuggestions(_)
            | P::SetPassengers(_)
            | P::Cooldown(_)
            | P::Login(_)
            | P::Respawn(_)
            | P::StartConfiguration(_)
            /* Not the HUD: what azalea drops from a window's contents, which tools::received puts back. */
            /* Not the HUD either: a sound, which a press may be waiting on. */
            | P::Sound(_)
            | P::OpenScreen(_)
            | P::ContainerSetContent(_)
            | P::SetCursorItem(_)
    )
}

pub struct Objective {
    pub title: FormattedText,
}

pub struct Score {
    pub value: i32,
    pub display: Option<FormattedText>,
}

pub struct Bar {
    pub id: u128,
    pub name: FormattedText,
    pub progress: f32,
    pub color: BossBarColor,
    pub overlay: BossBarOverlay,
}

struct Clock {
    total_ticks: u64,
    rate: f32,
    at_tick: u64,
}

#[derive(Default)]
pub struct Hud {
    pub objectives: HashMap<String, Objective>,
    /// The objective in the list, sidebar and below-name slots, in that order. The team-coloured
    /// sidebars are not kept: nothing reads them.
    displayed: [Option<String>; 3],
    /// Owner, then objective.
    scores: HashMap<String, HashMap<String, Score>>,
    /// In the order the server added them, which is the order the client stacks them in.
    pub bars: Vec<Bar>,
    clocks: HashMap<u32, Clock>,
    dimension: Option<(DimensionKind, Identifier)>,
    rain: f32,
    thunder: f32,
    /// The players the tab list shows. azalea keeps every player it was told about, and a server
    /// that hides one from the list -- an NPC, a vanished moderator -- still tells the client.
    listed: HashSet<u128>,
    completions: HashMap<u32, oneshot::Sender<Suggestions>>,
    next_completion: u32,
    /// Who rides what, by network id: a vehicle and its passengers. azalea drops the packet that
    /// says so, and a vehicle that has since left the world can still be in here, so a reader
    /// checks the vehicle is loaded before believing it.
    passengers: HashMap<i32, Vec<i32>>,
    /// The tick each cooling cooldown group comes free on, counted in the bot's own ticks the way
    /// the client counts them.
    cooldowns: HashMap<String, u64>,
    pub logins: u64,
    /// The end credits are up. The client shows them for any win-game event, whatever its value,
    /// and they hold the player outside every world until it asks to respawn.
    pub credits: bool,
    /// The dialog on screen, with what its inputs hold. Kept even when its definition could not be
    /// read, because keys still go to it and not to the game.
    pub dialog: Option<Open>,
    pub commands: Option<ClientboundCommands>,
}

pub enum Slot {
    List,
    Sidebar,
    BelowName,
}

impl Hud {
    /// Entries on the objective in a slot, highest score first and then by name, the order the
    /// sidebar draws them in.
    pub fn board(&self, slot: Slot) -> Option<(&Objective, Vec<(&str, &Score)>)> {
        let name = self.displayed[slot as usize].as_ref()?;
        let objective = self.objectives.get(name)?;

        let mut entries: Vec<(&str, &Score)> = self
            .scores
            .iter()
            .filter_map(|(owner, scores)| scores.get(name).map(|score| (owner.as_str(), score)))
            .collect();
        entries.sort_by(|a, b| b.1.value.cmp(&a.1.value).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));

        Some((objective, entries))
    }

    /// The vehicle an entity sits on, by network id.
    pub fn vehicle_of(&self, passenger: i32) -> Option<i32> {
        self.passengers.iter().find(|(_, riders)| riders.contains(&passenger)).map(|(vehicle, _)| *vehicle)
    }

    /// Ticks until a cooldown group comes free, or None when it is free now.
    pub fn cooldown_left(&self, group: &str, ticks_now: u64) -> Option<u64> {
        self.cooldowns.get(group).map(|end| end.saturating_sub(ticks_now)).filter(|left| *left > 0)
    }

    pub fn listed(&self, uuid: u128) -> bool {
        self.listed.contains(&uuid)
    }

    /// The dimension the player is in: its type, for the clock and the sky, and its name.
    pub fn dimension(&self) -> Option<&(DimensionKind, Identifier)> {
        self.dimension.as_ref()
    }

    /// The clock by its protocol id, run forward by the ticks since the server last set it -- which
    /// is what the client does between the packets, sent once a second.
    pub fn clock_ticks(&self, clock: u32, ticks_now: u64) -> Option<u64> {
        self.clocks.get(&clock).map(|clock| {
            let elapsed = ticks_now.saturating_sub(clock.at_tick) as f64 * f64::from(clock.rate);
            clock.total_ticks + elapsed as u64
        })
    }

    pub fn raining(&self) -> f32 {
        self.rain
    }

    /// Thunder as the game weighs it: a storm without rain is not one.
    pub fn thundering(&self) -> f32 {
        self.thunder * self.rain
    }

    /// Ask the server to complete a command; the answer comes back through [`apply`].
    pub fn ask(&mut self) -> (u32, oneshot::Receiver<Suggestions>) {
        self.next_completion = self.next_completion.wrapping_add(1);
        let (answer, answered) = oneshot::channel();
        self.completions.insert(self.next_completion, answer);
        (self.next_completion, answered)
    }

    pub fn forget(&mut self, id: u32) {
        self.completions.remove(&id);
    }
}

/// One packet the ECS handed over, applied to the connection it arrived on.
pub fn apply(bot: &Bot, packet: &ClientboundGamePacket) {
    use ClientboundGamePacket as P;

    match packet {
        P::SetActionBarText(p) => feeds::action_bar(bot, "actionbar", &p.text),
        P::SetTitleText(p) => feeds::title(bot, "title", &p.text),
        P::SetSubtitleText(p) => feeds::title(bot, "subtitle", &p.text),
        P::ShowDialog(p) => show_dialog(bot, dialog(bot, &p.dialog)),
        P::ClearDialog(_) => {
            if let Some(game) = bot.game.borrow().as_ref() {
                game.hud.borrow_mut().dialog = None;
            }
            feeds::dialog_closed(bot);
        }
        P::Sound(p) => feeds::sound(bot, match &p.sound {
            Holder::Reference(sound) => sound.to_str().to_owned(),
            Holder::Direct(custom) => custom.sound_id.to_string(),
        }),
        _ => {
            let game = bot.game.borrow();
            let Some(game) = game.as_ref() else { return };
            let mut hud = game.hud.borrow_mut();
            let ticks = *bot.ticks.borrow();
            keep(&mut hud, packet, ticks);

            /* A switch through a proxy ends in a login on the same connection, which azalea
            announces with no event: its spawn event is sent once per connection. */
            match packet {
                P::StartConfiguration(_) => game.reconfiguring(),
                P::Login(_) if hud.logins > 1 => game.arrived(),
                _ => {}
            }
        }
    }
}

/// A dialog coming up on screen, whichever way it came: sent by the server, or opened by the client
/// itself from a chat line's click. Both are the same dialog to a player, so both are the same here.
/// It is open even when it could not be read, because keys go to it either way.
pub fn show_dialog(bot: &Bot, dialog: Option<Value>) {
    if let Some(dialog) = &dialog {
        feeds::dialog(bot, dialog.clone());
    }
    if let Some(game) = bot.game.borrow().as_ref() {
        game.hud.borrow_mut().dialog = Some(Open::new(dialog.unwrap_or(Value::Null)));
    }
}

/// A dialog the server declared, by its id in the dialog registry.
pub fn registered_dialog(bot: &Bot, id: &str) -> Option<Value> {
    let game = bot.game.borrow();
    let client = &game.as_ref()?.client;
    client.with_registry_holder(|registries| {
        let entries = registries.extra.get(&Identifier::new("minecraft:dialog"))?;
        entries.map.get(&Identifier::new(id)).and_then(|nbt| serde_json::to_value(nbt).ok())
    })
}

fn keep(hud: &mut Hud, packet: &ClientboundGamePacket, ticks: u64) {
    use ClientboundGamePacket as P;

    match packet {
        P::SetObjective(p) => match &p.method {
            Method::Add { display_name, .. } | Method::Change { display_name, .. } => {
                hud.objectives.insert(p.objective_name.clone(), Objective { title: display_name.clone() });
            }
            Method::Remove => {
                hud.objectives.remove(&p.objective_name);
                for shown in &mut hud.displayed {
                    if shown.as_deref() == Some(p.objective_name.as_str()) {
                        *shown = None;
                    }
                }
                for scores in hud.scores.values_mut() {
                    scores.remove(&p.objective_name);
                }
            }
        },
        P::SetDisplayObjective(p) => {
            let slot = match p.slot {
                DisplaySlot::List => Slot::List,
                DisplaySlot::Sidebar => Slot::Sidebar,
                DisplaySlot::BelowName => Slot::BelowName,
                _ => return,
            };
            /* An empty name is how the server clears a slot. */
            hud.displayed[slot as usize] = (!p.objective_name.is_empty()).then(|| p.objective_name.clone());
        }
        P::SetScore(p) => {
            /* A VarInt on the wire, which azalea reads unsigned: a negative score is two's complement. */
            let score = Score { value: p.score as i32, display: p.display.clone() };
            hud.scores.entry(p.owner.clone()).or_default().insert(p.objective_name.clone(), score);
        }
        P::ResetScore(p) => match &p.objective_name {
            Some(objective) => {
                if let Some(scores) = hud.scores.get_mut(&p.owner) {
                    scores.remove(objective);
                }
            }
            None => {
                hud.scores.remove(&p.owner);
            }
        },
        P::BossEvent(p) => {
            let id = p.id.as_u128();
            match &p.operation {
                Operation::Add(add) => {
                    hud.bars.retain(|bar| bar.id != id);
                    hud.bars.push(Bar {
                        id,
                        name: add.name.clone(),
                        progress: add.progress,
                        color: add.style.color,
                        overlay: add.style.overlay,
                    });
                }
                Operation::Remove => hud.bars.retain(|bar| bar.id != id),
                operation => {
                    let Some(bar) = hud.bars.iter_mut().find(|bar| bar.id == id) else { return };
                    match operation {
                        Operation::UpdateProgress(progress) => bar.progress = *progress,
                        Operation::UpdateName(name) => bar.name = name.clone(),
                        Operation::UpdateStyle(style) => {
                            bar.color = style.color;
                            bar.overlay = style.overlay;
                        }
                        _ => {}
                    }
                }
            }
        }
        P::SetTime(p) => {
            for (clock, state) in &p.clock_updates {
                hud.clocks.insert(clock.protocol_id(), Clock {
                    total_ticks: state.total_ticks,
                    rate: state.rate,
                    at_tick: ticks,
                });
            }
        }
        P::GameEvent(p) => match p.event {
            /* The client's own reading of these: starting rain begins from none, stopping from full. */
            EventType::StartRaining => hud.rain = 0.0,
            EventType::StopRaining => hud.rain = 1.0,
            EventType::RainLevelChange => hud.rain = p.param.clamp(0.0, 1.0),
            EventType::ThunderLevelChange => hud.thunder = p.param.clamp(0.0, 1.0),
            EventType::WinGame => hud.credits = true,
            _ => {}
        },
        P::PlayerInfoUpdate(p) => {
            if p.actions.update_listed {
                for entry in &p.entries {
                    let uuid = entry.profile.uuid.as_u128();
                    if entry.listed {
                        hud.listed.insert(uuid);
                    } else {
                        hud.listed.remove(&uuid);
                    }
                }
            }
        }
        P::SetPassengers(p) => {
            if p.passengers.is_empty() {
                hud.passengers.remove(&p.vehicle.0);
            } else {
                hud.passengers.insert(p.vehicle.0, p.passengers.iter().map(|id| id.0).collect());
            }
        }
        P::Cooldown(p) => {
            /* A duration of zero is how the server ends one early. */
            let group = p.cooldown_group.to_string();
            if p.duration == 0 {
                hud.cooldowns.remove(&group);
            } else {
                hud.cooldowns.insert(group, ticks + u64::from(p.duration));
            }
        }
        P::PlayerInfoRemove(p) => {
            for uuid in &p.profile_ids {
                hud.listed.remove(&uuid.as_u128());
            }
        }
        P::Commands(p) => hud.commands = Some(p.clone()),
        P::CommandSuggestions(p) => {
            if let Some(answer) = hud.completions.remove(&p.id) {
                let _ = answer.send(p.suggestions.clone());
            }
        }
        P::Login(p) => {
            /*
            A login is a new world on a new connection as far as the client is concerned: the
            scoreboard, the boss bars and the weather belong to the server it was on, and a switch
            through a proxy would otherwise show the last backend's sidebar on the next one.
            */
            let logins = hud.logins + 1;
            *hud = Hud { logins, ..Hud::default() };
            hud.dimension = Some((p.common.dimension_type, p.common.dimension.clone()));
        }
        P::Respawn(p) => {
            /* A new level on the client, which starts dry and with nobody riding anything until the server says otherwise. */
            hud.credits = false;
            hud.rain = 0.0;
            hud.thunder = 0.0;
            hud.passengers.clear();
            /* The client makes a new player on respawn, and the cooldowns belonged to the old one. */
            hud.cooldowns.clear();
            hud.dimension = Some((p.common.dimension_type, p.common.dimension.clone()));
        }
        _ => {}
    }
}

/// The dialog a packet means, as JSON.
///
/// `/dialog show` names a dialog the datapack declared, and then the packet carries nothing but an
/// index into the `minecraft:dialog` registry the server sent while configuring. Reading only the
/// inline shape dropped every dialog a datapack declares.
fn dialog(bot: &Bot, dialog: &Holder<azalea::registry::data::Dialog, simdnbt::owned::Nbt>) -> Option<Value> {
    match dialog {
        Holder::Direct(nbt) => serde_json::to_value(nbt).ok(),
        Holder::Reference(entry) => {
            let game = bot.game.borrow();
            let client = &game.as_ref()?.client;
            client.with_registry_holder(|registries| {
                entry.resolve(registries).and_then(|(_, nbt)| serde_json::to_value(nbt).ok())
            })
        }
    }
}
