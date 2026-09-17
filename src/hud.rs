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
use azalea::protocol::packets::game::c_set_objective::Method;
use azalea::protocol::packets::game::c_set_player_team::{Method as TeamMethod, Parameters};
use azalea::registry::data::DimensionKind;
use azalea::registry::{DataRegistry, Holder};
use azalea::block::BlockTrait;
use azalea::core::entity_id::MinecraftEntityId;
use azalea::core::sound::CustomSound;
use azalea::registry::builtin::{BlockEntityKind, BlockKind, SoundEvent};
use azalea::{BlockPos, FormattedText, Identifier};
use azalea_chat::base_component::BaseComponent;
use azalea_chat::numbers::NumberFormat;
use azalea_chat::style::{ChatFormatting, Style, TextColor};
use azalea_chat::text_component::TextComponent;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::bot::Bot;
use crate::dialog::Open;
use crate::editors::{BlockEntities, CommandEditor, SignEditor, Tracked, sign_face};
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
            | P::SetPlayerTeam(_)
            | P::SetTime(_)
            | P::GameEvent(_)
            | P::ShowDialog(_)
            | P::ClearDialog(_)
            /* The command tree, which says whether the client would run a command a dialog asks for. */
            | P::Commands(_)
            /* Block entities, which a sign's text and a command block's command are only in, and the editor for a sign. */
            | P::LevelChunkWithLight(_)
            | P::ForgetLevelChunk(_)
            | P::BlockEntityData(_)
            | P::OpenSignEditor(_)
            /* Not the HUD: the one that says the player is an operator, which azalea reads and drops. */
            | P::EntityEvent(_)
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
            | P::SoundEntity(_)
            | P::OpenScreen(_)
            | P::ContainerSetContent(_)
            | P::SetCursorItem(_)
            /* Nor these: a hit the server registered, which a swing waits to hear of. */
            | P::HurtAnimation(_)
            | P::DamageEvent(_)
    )
}

pub struct Objective {
    pub title: FormattedText,
    /// How its scores are drawn, when it says: hidden, as fixed text, or as the number in a style.
    pub number_format: Option<NumberFormat>,
}

pub struct Score {
    pub value: i32,
    pub display: Option<FormattedText>,
    /// The entry's own format, which wins over the objective's.
    pub number_format: Option<NumberFormat>,
}

/// What a team does to the names of its members wherever they are drawn.
pub struct Team {
    color: ChatFormatting,
    prefix: FormattedText,
    suffix: FormattedText,
}

impl From<&Parameters> for Team {
    fn from(parameters: &Parameters) -> Team {
        Team {
            color: parameters.color,
            prefix: parameters.player_prefix.clone(),
            suffix: parameters.player_suffix.clone(),
        }
    }
}

/// A line of a board as the client draws it: the name, and the score column beside it.
pub struct Line {
    pub name: FormattedText,
    pub score: i32,
    pub value: FormattedText,
}

/// The display slots the protocol numbers: the list, the sidebar, below names, and a sidebar per
/// team colour after those.
const DISPLAY_SLOTS: usize = 19;
const FIRST_TEAM_SLOT: usize = 3;

/// The most lines a sidebar draws.
const SIDEBAR_LINES: usize = 15;

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
    /// The objective in each display slot, as the protocol numbers them. The client draws a
    /// team-coloured sidebar in place of the plain one for a player whose team wears that colour.
    displayed: [Option<String>; DISPLAY_SLOTS],
    /// Owner, then objective.
    scores: HashMap<String, HashMap<String, Score>>,
    teams: HashMap<String, Team>,
    /// The team each entry is on: a player's name, or an owner the server made up for a line.
    members: HashMap<String, String>,
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
    pub block_entities: BlockEntities,
    /// The sign editor and the command block editor, while one is open.
    pub sign_editor: Option<SignEditor>,
    pub command_editor: Option<CommandEditor>,
    /// The operator level the server last gave the player, from 0 to 4. azalea keeps a component
    /// for it and never sets it, so it reads 0 for an operator too.
    pub permission_level: u8,
    /// The player's own entity id, from the login. The operator level arrives straight after it,
    /// before azalea has given the player the component that would say the same.
    player_id: Option<MinecraftEntityId>,
    /// The server takes only signed chat, which it says at login. This bot never has a key to sign
    /// with, so plain chat to such a server is dropped.
    pub signed_chat_only: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Slot {
    List,
    Sidebar,
    BelowName,
}

impl Hud {
    /// The board in a slot as the client draws it for the player named `me`: the lines in order,
    /// highest score first and then by owner.
    ///
    /// The sidebar is the one the player's team colour names when that slot holds an objective, and
    /// the plain one otherwise; it leaves out the owners starting with '#', stops at fifteen, and
    /// draws each name between its team's prefix and suffix in the team's colour. The list and the
    /// name below a player are drawn elsewhere, without any of that. The score column is whatever
    /// the number format makes of the score: the entry's own, else the objective's, else the slot's
    /// default style on the number.
    pub fn board(&self, slot: Slot, me: &str) -> Option<(&Objective, Vec<Line>)> {
        let name = self.objective_in(slot, me)?;
        let objective = self.objectives.get(name)?;

        let mut entries: Vec<(&str, &Score)> = self
            .scores
            .iter()
            .filter_map(|(owner, scores)| scores.get(name).map(|score| (owner.as_str(), score)))
            .collect();
        entries.sort_by(|a, b| b.1.value.cmp(&a.1.value).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));

        let default_style = match slot {
            Slot::Sidebar => Style::default().color(TextColor::try_from(ChatFormatting::Red).ok()),
            Slot::List => Style::default().color(TextColor::try_from(ChatFormatting::Yellow).ok()),
            Slot::BelowName => Style::default(),
        };
        let line = |(owner, score): (&str, &Score)| {
            let name = score.display.clone().unwrap_or_else(|| FormattedText::from(owner));
            Line {
                name: if slot == Slot::Sidebar { self.team_name(owner, name) } else { name },
                score: score.value,
                value: formatted(score, objective, &default_style),
            }
        };

        let lines = match slot {
            Slot::Sidebar => {
                entries.into_iter().filter(|(owner, _)| !owner.starts_with('#')).take(SIDEBAR_LINES).map(line).collect()
            }
            _ => entries.into_iter().map(line).collect(),
        };
        Some((objective, lines))
    }

    fn objective_in(&self, slot: Slot, me: &str) -> Option<&String> {
        if slot == Slot::Sidebar
            && let Some(team) = self.team_of(me)
            && let Some(slot) = team_slot(team.color)
            && let Some(objective) = self.displayed[slot].as_ref()
        {
            return Some(objective);
        }
        self.displayed[slot as usize].as_ref()
    }

    fn team_of(&self, member: &str) -> Option<&Team> {
        self.teams.get(self.members.get(member)?)
    }

    /// A name as PlayerTeam.formatNameForTeam draws it: between the team's prefix and suffix, in
    /// the team's colour unless that is reset, and as it is off any team.
    fn team_name(&self, owner: &str, name: FormattedText) -> FormattedText {
        let Some(team) = self.team_of(owner) else { return name };
        let mut style = Style::default();
        if team.color != ChatFormatting::Reset {
            match TextColor::try_from(team.color) {
                Ok(color) => style.color = Some(color),
                Err(_) => style.apply_formatting(&team.color),
            }
        }
        FormattedText::Text(TextComponent {
            base: BaseComponent { siblings: vec![team.prefix.clone(), name, team.suffix.clone()], style: Box::new(style) },
            text: String::new(),
        })
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
        P::LevelChunkWithLight(p) => {
            let game = bot.game.borrow();
            let Some(game) = game.as_ref() else { return };
            let client = &game.client;
            let mut hud = game.hud.borrow_mut();
            hud.block_entities.forget_chunk(p.x, p.z);
            for entity in &p.chunk_data.block_entities {
                let at = BlockPos::new(
                    p.x * 16 + i32::from(entity.packed_xz >> 4),
                    i32::from(entity.y as i16),
                    p.z * 16 + i32::from(entity.packed_xz & 15),
                );
                hud.block_entities.keep(at, tracked(client, at, entity.kind, &entity.data));
            }
        }
        P::ForgetLevelChunk(p) => {
            if let Some(game) = bot.game.borrow().as_ref() {
                game.hud.borrow_mut().block_entities.forget_chunk(p.pos.x, p.pos.z);
            }
        }
        P::BlockEntityData(p) => {
            let game = bot.game.borrow();
            let Some(game) = game.as_ref() else { return };
            let client = &game.client;
            let mut hud = game.hud.borrow_mut();
            let kept = tracked(client, p.pos, p.block_entity_type, &p.tag);
            /* The editor that opened before the block's command arrived is filled in now, as the client fills it. */
            if let Some(editor) = hud.command_editor.as_mut().filter(|editor| editor.at == p.pos) {
                let state = client.world().read().get_block_state(p.pos).unwrap_or_default();
                let conditional = Box::<dyn BlockTrait>::from(state).get_property("conditional") == Some("true");
                editor.load(&kept.data, kept.block.as_deref().unwrap_or_default(), conditional);
            }
            hud.block_entities.keep(p.pos, kept);
        }
        P::EntityEvent(p) => {
            /* Events 24 to 28 set the operator level, 0 to 4, and only ever for the player's own entity. */
            if let 24..=28 = p.event_id
                && let Some(game) = bot.game.borrow().as_ref()
            {
                let mut hud = game.hud.borrow_mut();
                if hud.player_id == Some(p.entity_id) {
                    hud.permission_level = p.event_id - 24;
                }
            }
        }
        P::OpenSignEditor(p) => {
            let game = bot.game.borrow();
            let Some(game) = game.as_ref() else { return };
            let client = &game.client;
            let mut hud = game.hud.borrow_mut();
            let block = block_name(client, p.pos);
            let face = if p.is_front_text { "front_text" } else { "back_text" };
            let lines = hud.block_entities.at(p.pos, &block).map(|sign| sign_face(&sign.data, face).0).unwrap_or_default();
            let mut kept = [String::new(), String::new(), String::new(), String::new()];
            for (line, text) in kept.iter_mut().zip(lines) {
                *line = text;
            }
            hud.sign_editor = Some(SignEditor { at: p.pos, front: p.is_front_text, hanging: block.ends_with("hanging_sign"), lines: kept });
        }
        P::Sound(p) => feeds::sound(bot, sound_id(&p.sound)),
        /* A sound played at an entity rather than at a place, which a plugin does with playSound(player, ...). */
        P::SoundEntity(p) => feeds::sound(bot, sound_id(&p.sound)),
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

fn sound_id(sound: &Holder<SoundEvent, CustomSound>) -> String {
    match sound {
        Holder::Reference(sound) => sound.to_str().to_owned(),
        Holder::Direct(custom) => custom.sound_id.to_string(),
    }
}

fn keep(hud: &mut Hud, packet: &ClientboundGamePacket, ticks: u64) {
    use ClientboundGamePacket as P;

    match packet {
        P::SetObjective(p) => match &p.method {
            Method::Add { display_name, number_format, .. } | Method::Change { display_name, number_format, .. } => {
                let objective = Objective { title: display_name.clone(), number_format: number_format.clone() };
                hud.objectives.insert(p.objective_name.clone(), objective);
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
            /* An empty name is how the server clears a slot. */
            hud.displayed[p.slot as usize] = (!p.objective_name.is_empty()).then(|| p.objective_name.clone());
        }
        P::SetScore(p) => {
            /* A VarInt on the wire, which azalea reads unsigned: a negative score is two's complement. */
            let score = Score { value: p.score as i32, display: p.display.clone(), number_format: p.number_format.clone() };
            hud.scores.entry(p.owner.clone()).or_default().insert(p.objective_name.clone(), score);
        }
        P::SetPlayerTeam(p) => match &p.method {
            TeamMethod::Add((parameters, members)) => {
                hud.teams.insert(p.name.clone(), Team::from(parameters));
                for member in members {
                    hud.members.insert(member.clone(), p.name.clone());
                }
            }
            TeamMethod::Remove => {
                hud.teams.remove(&p.name);
                hud.members.retain(|_, team| *team != p.name);
            }
            /* A team the client was never told of is left alone, as the client leaves it. */
            TeamMethod::Change(parameters) => {
                if let Some(team) = hud.teams.get_mut(&p.name) {
                    *team = Team::from(parameters);
                }
            }
            TeamMethod::Join(members) => {
                if hud.teams.contains_key(&p.name) {
                    for member in members {
                        hud.members.insert(member.clone(), p.name.clone());
                    }
                }
            }
            TeamMethod::Leave(members) => {
                for member in members {
                    if hud.members.get(member) == Some(&p.name) {
                        hud.members.remove(member);
                    }
                }
            }
        },
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
            *hud = Hud { logins, player_id: Some(p.player_id), signed_chat_only: p.enforces_secure_chat, ..Hud::default() };
            hud.dimension = Some((p.common.dimension_type, p.common.dimension.clone()));
        }
        P::Respawn(p) => {
            /* A new level on the client, which starts dry and with nobody riding anything until the server says otherwise. */
            hud.credits = false;
            hud.block_entities.clear();
            hud.sign_editor = None;
            hud.command_editor = None;
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

/// The sidebar slot a team colour names, as DisplaySlot.teamColorToSlot does: none for a
/// formatting such as bold or reset, which is no colour.
fn team_slot(color: ChatFormatting) -> Option<usize> {
    (!color.is_format()).then(|| FIRST_TEAM_SLOT + color as usize)
}

/// The score column as PlayerScoreEntry.formatValue draws it.
fn formatted(score: &Score, objective: &Objective, default_style: &Style) -> FormattedText {
    match score.number_format.as_ref().or(objective.number_format.as_ref()) {
        Some(NumberFormat::Blank) => FormattedText::from(String::new()),
        Some(NumberFormat::Fixed { value }) => value.clone(),
        Some(NumberFormat::Styled { style }) => styled(score.value, &nbt_style(style)),
        None => styled(score.value, default_style),
    }
}

fn styled(value: i32, style: &Style) -> FormattedText {
    FormattedText::Text(TextComponent { base: BaseComponent::new().with_style(style.clone()), text: value.to_string() })
}

/// A style as a styled number format carries it, which is network NBT: the colour and the font by
/// name, and the flags as bytes. Nothing at all is the empty style.
fn nbt_style(nbt: &simdnbt::owned::Nbt) -> Style {
    let flag = |name| nbt.byte(name).map(|flag| flag != 0);
    Style::default()
        .color(nbt.string("color").and_then(|color| TextColor::parse(&color.to_str())))
        .bold(flag("bold"))
        .italic(flag("italic"))
        .underlined(flag("underlined"))
        .strikethrough(flag("strikethrough"))
        .obfuscated(flag("obfuscated"))
        .font(nbt.string("font").map(|font| font.to_str().into_owned()))
}

fn tracked(client: &azalea::Client, at: BlockPos, kind: BlockEntityKind, data: &simdnbt::owned::Nbt) -> Tracked {
    let block = client.world().read().get_block_state(at).map(|state| BlockKind::from(state).to_str().to_owned());
    Tracked { kind: kind.to_str().to_owned(), block, data: serde_json::to_value(data).unwrap_or_default() }
}

pub fn block_name(client: &azalea::Client, at: BlockPos) -> String {
    client.world().read().get_block_state(at).map(|state| BlockKind::from(state).to_str().to_owned()).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use azalea::FormattedText;
    use azalea::buf::AzBuf;
    use azalea::core::objectives::ObjectiveCriteria;
    use azalea::protocol::packets::game::c_set_display_objective::DisplaySlot;
    use azalea::protocol::packets::game::c_set_objective::Method;
    use azalea::protocol::packets::game::c_set_player_team::{CollisionRule, Method as TeamMethod, NameTagVisibility, Parameters};
    use azalea::protocol::packets::game::{
        ClientboundGamePacket, ClientboundSetDisplayObjective, ClientboundSetObjective, ClientboundSetPlayerTeam,
        ClientboundSetScore,
    };
    use azalea_chat::numbers::NumberFormat;
    use azalea_chat::style::{ChatFormatting, TextColor};
    use serde_json::json;

    use super::{Hud, Slot, keep, nbt_style};
    use crate::text::component;

    /// A set-objective packet as 26.1.2 writes it: the name, the method as a byte, the title as a
    /// string tag, the criteria, then the number format behind a presence boolean.
    fn set_objective(method: u8, number_format: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1, b'q', method, 0x08, 0, 5];
        bytes.extend_from_slice(b"Quest");
        bytes.push(0);
        bytes.extend_from_slice(number_format);
        bytes
    }

    fn read_objective(bytes: &[u8]) -> ClientboundSetObjective {
        let mut buf = Cursor::new(bytes);
        let packet = ClientboundSetObjective::azalea_read(&mut buf).unwrap();
        assert_eq!(buf.position(), bytes.len() as u64, "every byte of the packet is read");
        packet
    }

    #[test]
    fn an_objective_without_a_number_format_sends_the_presence_boolean_alone() {
        let packet = read_objective(&set_objective(2, &[0]));
        assert!(matches!(packet.method, Method::Change { number_format: None, .. }));
    }

    #[test]
    fn a_fixed_number_format_follows_its_presence_boolean_and_kind() {
        let packet = read_objective(&set_objective(0, &[1, 2, 0x08, 0, 3, b'5', b'0', b'g']));
        let Method::Add { number_format, .. } = packet.method else { panic!("not an add") };
        assert_eq!(number_format, Some(NumberFormat::Fixed { value: FormattedText::from("50g") }));
    }

    /// The style is network NBT, whose root compound carries no name.
    #[test]
    fn a_styled_number_format_carries_its_style_as_unnamed_nbt() {
        let mut nbt = vec![0x0A, 0x08, 0, 5];
        nbt.extend_from_slice(b"color");
        nbt.extend_from_slice(&[0, 3, b'r', b'e', b'd', 0]);
        let mut number_format = vec![1, 1];
        number_format.extend_from_slice(&nbt);

        let packet = read_objective(&set_objective(0, &number_format));
        let Method::Add { number_format: Some(NumberFormat::Styled { style }), .. } = packet.method else {
            panic!("not styled")
        };
        assert_eq!(nbt_style(&style).color, TextColor::try_from(ChatFormatting::Red).ok());
        assert_eq!(nbt_style(&simdnbt::owned::Nbt::None), azalea_chat::style::Style::default());
    }

    fn apply(hud: &mut Hud, packets: Vec<ClientboundGamePacket>) {
        for packet in packets {
            keep(hud, &packet, 0);
        }
    }

    fn objective(name: &str, number_format: Option<NumberFormat>) -> ClientboundGamePacket {
        ClientboundGamePacket::SetObjective(ClientboundSetObjective {
            objective_name: name.into(),
            method: Method::Add { display_name: FormattedText::from(name), render_type: ObjectiveCriteria::Integer, number_format },
        })
    }

    fn display(slot: DisplaySlot, objective: &str) -> ClientboundGamePacket {
        ClientboundGamePacket::SetDisplayObjective(ClientboundSetDisplayObjective { slot, objective_name: objective.into() })
    }

    fn score(owner: &str, objective: &str, value: i32) -> ClientboundGamePacket {
        ClientboundGamePacket::SetScore(ClientboundSetScore {
            owner: owner.into(),
            objective_name: objective.into(),
            score: value as u32,
            display: None,
            number_format: None,
        })
    }

    fn parameters(prefix: &str, suffix: &str, color: ChatFormatting) -> Parameters {
        Parameters {
            display_name: FormattedText::default(),
            options: 0,
            nametag_visibility: NameTagVisibility::Always,
            collision_rule: CollisionRule::Always,
            color,
            player_prefix: FormattedText::from(prefix),
            player_suffix: FormattedText::from(suffix),
        }
    }

    fn team(name: &str, method: TeamMethod) -> ClientboundGamePacket {
        ClientboundGamePacket::SetPlayerTeam(ClientboundSetPlayerTeam { name: name.into(), method })
    }

    fn quest_log() -> Hud {
        let mut hud = Hud::default();
        apply(&mut hud, vec![
            objective("lines", Some(NumberFormat::Blank)),
            display(DisplaySlot::Sidebar, "lines"),
            team("l3", TeamMethod::Add((parameters("Harvest wheat", "3/10", ChatFormatting::Reset), vec!["§7".into()]))),
            score("§7", "lines", 3),
            team("l2", TeamMethod::Add((parameters("", "", ChatFormatting::Gray), vec!["§8".into()]))),
            score("§8", "lines", 2),
            team("l1", TeamMethod::Add((parameters("» ", "", ChatFormatting::Reset), vec!["§9".into()]))),
            ClientboundGamePacket::SetScore(ClientboundSetScore {
                owner: "§9".into(),
                objective_name: "lines".into(),
                score: 1,
                display: Some(FormattedText::from("Reward")),
                number_format: Some(NumberFormat::Fixed { value: FormattedText::from("50g") }),
            }),
            score("#hidden", "lines", 9),
        ]);
        hud
    }

    #[test]
    fn a_sidebar_draws_each_name_in_its_team_and_the_score_as_the_format_says() {
        let hud = quest_log();
        let (objective, lines) = hud.board(Slot::Sidebar, "me").unwrap();

        assert_eq!(objective.title.to_string(), "lines");
        let drawn: Vec<(String, i32, String)> =
            lines.iter().map(|line| (line.name.to_string(), line.score, line.value.to_string())).collect();
        assert_eq!(drawn, vec![
            ("Harvest wheat§73/10".into(), 3, "".into()),
            ("§8".into(), 2, "".into()),
            ("» Reward".into(), 1, "50g".into()),
        ]);
        assert_eq!(component(&lines[1].name), json!({"text": "", "color": "gray", "extra": ["", "§8", ""]}));
        assert_eq!(component(&lines[0].value), json!(""));
    }

    #[test]
    fn the_list_and_below_name_slots_draw_every_owner_bare_with_the_slots_own_style() {
        let mut hud = quest_log();
        apply(&mut hud, vec![objective("stats", None), display(DisplaySlot::List, "stats"), score("#hidden", "stats", 9)]);
        apply(&mut hud, vec![display(DisplaySlot::BelowName, "lines"), score("Steve", "lines", 4)]);

        let (_, lines) = hud.board(Slot::List, "me").unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(component(&lines[0].name), json!("#hidden"));
        assert_eq!(component(&lines[0].value), json!({"text": "9", "color": "yellow"}));

        let (_, lines) = hud.board(Slot::BelowName, "me").unwrap();
        let names: Vec<String> = lines.iter().map(|line| line.name.to_string()).collect();
        assert_eq!(names, vec!["#hidden", "Steve", "§7", "§8", "Reward"]);
        assert_eq!(component(&lines[1].name), json!("Steve"));
    }

    #[test]
    fn a_sidebar_stops_at_fifteen_lines_after_leaving_out_the_hidden() {
        let mut hud = Hud::default();
        apply(&mut hud, vec![objective("many", None), display(DisplaySlot::Sidebar, "many"), score("#top", "many", 99)]);
        apply(&mut hud, (0..16).map(|n| score(&format!("line{n:02}"), "many", n)).collect());

        let (_, lines) = hud.board(Slot::Sidebar, "me").unwrap();
        assert_eq!(lines.len(), 15);
        assert_eq!(lines[0].name.to_string(), "line15");
        assert_eq!(component(&lines[0].value), json!({"text": "15", "color": "red"}));
        assert_eq!(lines[14].name.to_string(), "line01");
    }

    #[test]
    fn the_sidebar_is_the_one_the_players_team_colour_names_when_that_slot_is_filled() {
        let mut hud = quest_log();
        apply(&mut hud, vec![
            objective("red", None),
            display(DisplaySlot::TeamRed, "red"),
            team("reds", TeamMethod::Add((parameters("", "", ChatFormatting::Red), vec!["me".into()]))),
            team("bold", TeamMethod::Add((parameters("", "", ChatFormatting::Bold), vec!["other".into()]))),
        ]);

        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().0.title.to_string(), "red");
        assert_eq!(hud.board(Slot::Sidebar, "other").unwrap().0.title.to_string(), "lines");
        assert_eq!(hud.board(Slot::Sidebar, "nobody").unwrap().0.title.to_string(), "lines");

        apply(&mut hud, vec![display(DisplaySlot::TeamRed, "")]);
        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().0.title.to_string(), "lines");
    }

    #[test]
    fn membership_follows_the_teams_own_rules() {
        let mut hud = quest_log();

        /* Leaving a team the entry is not on changes nothing; joining another moves it. */
        apply(&mut hud, vec![team("l2", TeamMethod::Leave(vec!["§7".into()]))]);
        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().1[0].name.to_string(), "Harvest wheat§73/10");
        apply(&mut hud, vec![team("l2", TeamMethod::Join(vec!["§7".into()]))]);
        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().1[0].name.to_string(), "§7");
        apply(&mut hud, vec![team("l2", TeamMethod::Leave(vec!["§7".into()]))]);
        assert_eq!(component(&hud.board(Slot::Sidebar, "me").unwrap().1[0].name), json!("§7"));

        /* A change keeps the members; a removal drops them with the team. */
        apply(&mut hud, vec![team("l1", TeamMethod::Change(parameters("- ", "", ChatFormatting::Gold)))]);
        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().1[2].name.to_string(), "- Reward");
        apply(&mut hud, vec![team("l1", TeamMethod::Remove)]);
        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().1[2].name.to_string(), "Reward");

        /* A team the client was never told of takes no members. */
        apply(&mut hud, vec![team("ghost", TeamMethod::Join(vec!["§9".into()]))]);
        assert_eq!(hud.board(Slot::Sidebar, "me").unwrap().1[2].name.to_string(), "Reward");
    }

    /// The colour is the game's ordinal, which numbers bold 17 and strikethrough 18.
    #[test]
    fn a_team_coloured_bold_on_the_wire_draws_its_names_bold() {
        let mut bytes = vec![1, b'b', 0, 0x08, 0, 0, 0, 0, 0, 17, 0x08, 0, 0, 0x08, 0, 0, 1, 3];
        bytes.extend_from_slice("§7".as_bytes());
        let mut buf = Cursor::new(bytes.as_slice());
        let packet = ClientboundSetPlayerTeam::azalea_read(&mut buf).unwrap();
        assert_eq!(buf.position(), bytes.len() as u64, "every byte of the packet is read");
        let TeamMethod::Add((parameters, _)) = &packet.method else { panic!("not an add") };
        assert_eq!(parameters.color, ChatFormatting::Bold);

        let mut hud = quest_log();
        apply(&mut hud, vec![ClientboundGamePacket::SetPlayerTeam(packet)]);
        let name = component(&hud.board(Slot::Sidebar, "me").unwrap().1[0].name);
        assert_eq!(name["bold"], json!(true));
        assert_eq!(name.get("strikethrough"), None);
    }
}
