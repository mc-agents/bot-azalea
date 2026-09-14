use azalea::block::{BlockState, BlockTrait};
use azalea::buf::AzBufVar;
use azalea::core::delta::LpVec3;
use azalea::core::entity_id::MinecraftEntityId;
use azalea::ecs::entity::Entity;
use azalea::ecs::query::{Has, Without};
use azalea::entity::dimensions::EntityDimensions;
use azalea::entity::metadata::{AbstractInsentient, BlockDisplayBlockState, CustomName, ItemDisplayItemStack, Player};
use azalea::entity::{EntityKindComponent, EntityUuid, LocalEntity, Position};
use azalea::protocol::packets::game::s_attack::ServerboundAttack;
use azalea::protocol::packets::game::s_interact::{InteractionHand, ServerboundInteract};
use azalea::protocol::packets::{Packet, ProtocolPacket};
use azalea::world::WorldName;
use azalea::{BlockPos, Client, FormattedText, Vec3};
use serde_json::{Value, json};

use super::approach::Approach;
use super::args::{integer, plain, point, text};
use super::text::{component, one_decimal};
use super::windows::swing;
use super::{Tool, alive, in_world, stacks, tick};
use crate::calls::{Answer, Failure};

/// Enough to tell a misspelling from an empty world without printing the whole level.
const MAX_LISTED: usize = 5;

/// Ticks between swings. The attack cooldown lands two hits in one tick as one, and this is the gap
/// the other kinds of bot leave, so a mob takes the same beating from any of them.
const INTERVAL_TICKS: u32 = 10;

/// An entity as the client sees it, at the moment it was looked at.
struct Seen {
    entity: Entity,
    kind: &'static str,
    /// The name a server gave it, else a player's name, else what it is.
    label: String,
    custom: Option<FormattedText>,
    position: Vec3,
    distance: f32,
    player: bool,
    mob: bool,
    /// What an item display holds up, or what a block display draws; null for every other entity.
    item: Value,
    block: Value,
}

impl Seen {
    /// The catalogue documents the filter as "player", "mob", or part of a name, so those two words
    /// are the whole category and not a substring of an id.
    fn matches(&self, query: &str) -> bool {
        let needle = stacks::needle(query);
        match needle.as_str() {
            "player" => self.player,
            "mob" => self.mob,
            _ => self.kind.contains(&needle) || self.label.to_lowercase().contains(&needle),
        }
    }

    /// How it reads in a sentence: the name, and what it is after it when a name hides that. An
    /// unnamed cow reads "cow" and not "cow (cow)".
    fn named(&self) -> String {
        if self.label == self.kind { self.label.clone() } else { format!("{} ({})", self.label, self.kind) }
    }

    fn describe(&self) -> Value {
        json!({
            "label": self.label,
            /* A nameplate is a HUD on a server that draws with glyphs, so the component travels too. */
            "labelComponent": self.custom.as_ref().map_or(Value::Null, component),
            "type": self.kind,
            "position": point(BlockPos::from(self.position)),
            "distance": (f64::from(self.distance) * 10.0 + 0.5).floor() / 10.0,
            "item": self.item,
            "block": self.block,
        })
    }
}

/// Every other entity in the bot's world, nearest first.
///
/// The distance is worked in single precision, the way the game measures one entity from another,
/// because the radius a caller gives is compared against that and a cow on the edge of it is in on
/// one kind of bot or out on both.
fn nearby(client: &Client) -> Vec<Seen> {
    let players = client.tab_list();
    let Some(world) = client.get_component::<WorldName>().map(|name| name.clone()) else {
        return Vec::new();
    };
    let from = client.position();

    let mut ecs = client.ecs.write();
    let mut query = ecs.query_filtered::<(
        Entity,
        &WorldName,
        &Position,
        &EntityKindComponent,
        Option<&CustomName>,
        Option<&EntityUuid>,
        Has<Player>,
        Has<AbstractInsentient>,
        (Option<&ItemDisplayItemStack>, Option<&BlockDisplayBlockState>),
    ), Without<LocalEntity>>();

    let mut seen: Vec<Seen> = query
        .iter(&ecs)
        .filter(|(_, name, ..)| **name == world)
        .map(|(entity, _, position, kind, custom, uuid, player, mob, (item, block))| {
            let kind = plain(kind.0.to_str());
            let custom = custom.and_then(|name| name.0.as_deref().cloned());
            let label = match (&custom, uuid.and_then(|uuid| players.get(uuid))) {
                (Some(custom), _) => custom.to_string(),
                (None, Some(info)) if player => info.profile.name.clone(),
                _ => kind.to_owned(),
            };
            let (dx, dy, dz) = ((from.x - position.x) as f32, (from.y - position.y) as f32, (from.z - position.z) as f32);

            Seen {
                entity,
                kind,
                label,
                custom,
                position: **position,
                distance: (dx * dx + dy * dy + dz * dz).sqrt(),
                player,
                mob,
                /*
                Every display is called item_display or block_display, and what tells a chair from
                a signpost is what it shows. The metadata is only there on the kind it belongs to.
                */
                item: item.map_or(Value::Null, |item| stacks::shown(&item.0)),
                block: block.map_or(Value::Null, |block| block_state(block.0)),
            }
        })
        .collect();

    seen.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    seen
}

/// A block state as its name and its properties, the way the game names both.
fn block_state(state: BlockState) -> Value {
    let block = Box::<dyn BlockTrait>::from(state);
    json!({"name": block.id(), "properties": block.property_map()})
}

/// The nearest entity answering to what a caller typed, or a refusal naming what was in range
/// instead. Being told what is nearby is the difference between "my NPC did not spawn" and "I
/// spelled its name wrong".
fn require(client: &Client, query: &str, max_distance: f64) -> Result<Seen, Failure> {
    let mut nearby: Vec<Seen> = nearby(client).into_iter().filter(|seen| f64::from(seen.distance) <= max_distance).collect();

    if let Some(index) = nearby.iter().position(|seen| seen.matches(query)) {
        return Ok(nearby.swap_remove(index));
    }

    let listed: Vec<String> = nearby
        .iter()
        .take(MAX_LISTED)
        .map(|seen| format!("{} ({} blocks away)", seen.label, one_decimal(f64::from(seen.distance))))
        .collect();
    let instead = if listed.is_empty() { "Nothing else is in range either.".to_owned() } else { format!("In range: {}.", listed.join(", ")) };

    Err(Failure::refused(
        "NO_SUCH_ENTITY",
        format!("Nothing within {} blocks is named like \"{query}\". {instead}", super::text::plain(max_distance)),
    ))
}

fn max_distance(args: &Value, fallback: f64) -> f64 {
    args["maxDistance"].as_f64().unwrap_or(fallback)
}

/// Where an entity is now, and where its eyes are; nothing once it has left the world.
fn whereabouts(client: &Client, entity: Entity) -> Option<(Vec3, Vec3)> {
    let position = **client.get_entity_component::<Position>(entity)?;
    let eyes = client.get_entity_component::<EntityDimensions>(entity).map_or(0.0, |dimensions| dimensions.eye_height);
    Some((position, position.up(f64::from(eyes))))
}

pub const FIND_ENTITY: Tool = Tool {
    name: "find-entity",
    run: |bot, args| {
        Box::pin(async move {
            let query = args["type"].as_str().map(str::to_owned);
            let max_distance = max_distance(&args, 16.0);
            let count = integer(&args, "count", 1)?.max(0) as usize;

            let entities = in_world(&bot, |game| {
                nearby(&game.client)
                    .into_iter()
                    .filter(|seen| f64::from(seen.distance) <= max_distance)
                    .filter(|seen| query.as_deref().is_none_or(|query| seen.matches(query)))
                    .take(count)
                    .map(|seen| seen.describe())
                    .collect::<Vec<_>>()
            })?;

            Ok(Answer::data(
                "find-entity",
                json!({"query": args["type"], "maxDistance": max_distance, "entities": entities}),
            ))
        })
    },
};

/// Swing at an entity, more than once when asked, walking after it before each swing so a mob that
/// backs off is followed rather than missed.
///
/// A target that dies partway through is the count that landed and not a failure: something that
/// went away because it was killed is the tool working.
pub const ATTACK_ENTITY: Tool = Tool {
    name: "attack-entity",
    run: |bot, args| {
        Box::pin(async move {
            let query = text(&args, "name")?.to_owned();
            let max_distance = max_distance(&args, 8.0);
            let times = integer(&args, "times", 1)?;

            let target = alive(&bot, |game| require(&game.client, &query, max_distance))??;
            let label = target.label;

            let mut approach = Approach::new();
            let mut landed = 0;
            let mut waiting = 0;

            loop {
                if waiting > 0 {
                    waiting -= 1;
                    tick(&bot).await;
                    continue;
                }

                let Some((position, eyes)) = in_world(&bot, |game| whereabouts(&game.client, target.entity))? else {
                    return Ok(Answer::text(format!(
                        "Hit {label} {landed} time(s) out of {times}; it left the world before the rest landed."
                    )));
                };
                if !approach.reached(&bot, position, &label)? {
                    tick(&bot).await;
                    continue;
                }

                alive(&bot, |game| attack(&game.client, target.entity, eyes))??;
                landed += 1;

                if landed == times {
                    return Ok(Answer::text(format!("Hit {label} {landed} time(s).")));
                }
                waiting = INTERVAL_TICKS;
                tick(&bot).await;
            }
        })
    },
};

/// Right-click an entity, which is what opens an NPC's dialogue or a villager's trades.
pub const INTERACT_ENTITY: Tool = Tool {
    name: "interact-entity",
    run: |bot, args| {
        Box::pin(async move {
            let query = text(&args, "name")?.to_owned();
            let max_distance = max_distance(&args, 8.0);

            let target = alive(&bot, |game| require(&game.client, &query, max_distance))??;

            let mut approach = Approach::new();
            loop {
                let Some((position, _)) = in_world(&bot, |game| whereabouts(&game.client, target.entity))? else {
                    return Err(Failure::refused("NO_SUCH_ENTITY", format!("{} left the world before it was reached.", target.named())));
                };
                if approach.reached(&bot, position, &target.label)? {
                    break;
                }
                tick(&bot).await;
            }

            alive(&bot, |game| interact(&game.client, target.entity))??;
            Ok(Answer::text(format!("Right-clicked {}.", target.named())))
        })
    },
};

/// One attack packet, with the entity id written as the VarInt the game reads.
///
/// azalea's attack packet writes the id as a four-byte int. The server cannot decode that and drops
/// the connection, so a bot that swung at a mob was thrown out of the world. The packet's id still
/// comes from azalea's own table; only the body is written here.
///
/// Upstream: `ServerboundAttack::entity_id` in azalea-protocol 0.16.0 (`s_attack.rs`) lacks the
/// `#[var]` its `ServerboundInteract` has. Once azalea marks it, `Client::attack` does this.
pub(super) fn attack(client: &Client, entity: Entity, eyes: Vec3) -> Result<(), Failure> {
    let target = *client
        .get_entity_component::<MinecraftEntityId>(entity)
        .ok_or_else(|| Failure::refused("NO_SUCH_ENTITY", "the entity left the world before it was hit."))?;

    let mut packet = Vec::new();
    let _ = ServerboundAttack { entity_id: target }.into_variant().id().azalea_write_var(&mut packet);
    let _ = target.azalea_write_var(&mut packet);

    client.look_at(eyes);
    client
        .with_raw_connection_mut(|mut connection| connection.net_conn().map(|network| network.write_raw(&packet).is_ok()))
        .filter(|written| *written)
        .ok_or_else(Failure::not_in_game)?;
    swing(client);
    Ok(())
}

/// One interact packet, at the eyes, as the game sends it.
///
/// azalea's own sends the entity's position where the game sends the click relative to the entity,
/// which puts the click far outside anything an armour stand or a plugin reading the location would
/// accept, and it sends the packet twice, the way an older version did.
///
/// Upstream: `handle_entity_interact` in azalea-client 0.16.0 (`plugins/interact/mod.rs`). Once it
/// subtracts the entity's position and sends once, `Client::entity_interact` does this.
pub(super) fn interact(client: &Client, entity: Entity) -> Result<(), Failure> {
    let lost = || Failure::refused("NO_SUCH_ENTITY", "the entity left the world before it was clicked.");
    let (_, eyes) = whereabouts(client, entity).ok_or_else(lost)?;
    let id = *client.get_entity_component::<MinecraftEntityId>(entity).ok_or_else(lost)?;
    let position = **client.get_entity_component::<Position>(entity).ok_or_else(lost)?;

    client.look_at(eyes);
    client.write_packet(ServerboundInteract {
        entity_id: id,
        hand: InteractionHand::MainHand,
        location: LpVec3::from(eyes - position),
        using_secondary_action: client.crouching(),
    });
    swing(client);
    Ok(())
}
