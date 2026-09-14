use std::collections::HashMap;

use azalea::block::{BlockState, BlockTrait};
use azalea::buf::AzBufVar;
use azalea::core::aabb::Aabb;
use azalea::core::delta::LpVec3;
use azalea::core::entity_id::MinecraftEntityId;
use azalea::ecs::entity::Entity;
use azalea::ecs::query::{Has, Without};
use azalea::entity::dimensions::EntityDimensions;
use azalea::entity::metadata::{
    AbstractBoat, AbstractDisplay, AbstractInsentient, AbstractLiving, AbstractMinecart, ArmorStand, ArmorStandMarker,
    BlockDisplayBlockState, CustomName, CustomNameVisible, Interaction, InteractionHeight, InteractionWidth, Invisible,
    ItemDisplayItemStack, Player, Text,
};
use azalea::entity::{Attributes, Dead, EntityKindComponent, EntityUuid, LocalEntity, Position, view_vector};
use azalea::interact::pick::pick_block;
use azalea::protocol::packets::game::s_attack::ServerboundAttack;
use azalea::protocol::packets::game::s_interact::{InteractionHand, ServerboundInteract};
use azalea::protocol::packets::{Packet, ProtocolPacket};
use azalea::world::WorldName;
use azalea::{BlockPos, Client, FormattedText, Vec3};
use serde_json::{Value, json};

use super::approach::Approach;
use super::args::{integer, plain, point};
use super::text::{component, one_decimal};
use super::windows::swing;
use super::{Tool, alive, in_world, stacks, tick};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};

/// Enough to tell a misspelling from an empty world without printing the whole level.
const MAX_LISTED: usize = 5;

/// Ticks between swings. The attack cooldown lands two hits in one tick as one, and this is the gap
/// the other kinds of bot leave, so a mob takes the same beating from any of them.
const INTERVAL_TICKS: u32 = 10;

/// How far below a label an entity's feet may be. A tall model's label sits well above its base.
const BELOW: f64 = 3.0;

/// How far to the side, so a label over one NPC is not read as belonging to its neighbour.
const ASIDE: f64 = 1.0;

/// An entity as the client sees it, at the moment it was looked at.
struct Seen {
    entity: Entity,
    id: Option<i32>,
    kind: &'static str,
    /// The name a server gave it, else a player's name, else what it is.
    label: String,
    custom: Option<FormattedText>,
    /// What it says, when it is text floating in the world rather than something to click.
    says: Option<FormattedText>,
    position: Vec3,
    bounds: Aabb,
    distance: f32,
    player: bool,
    mob: bool,
    pickable: bool,
    display: bool,
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

    /// A label with something a player could read in it.
    fn readable(&self) -> Option<String> {
        self.says.as_ref().map(ToString::to_string).filter(|said| !said.trim().is_empty())
    }

    fn describe(&self, nameplate: Option<&Seen>) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            /* A nameplate is a HUD on a server that draws with glyphs, so the component travels too. */
            "labelComponent": self.custom.as_ref().map_or(Value::Null, component),
            "nameplate": nameplate.and_then(|plate| plate.says.as_ref()).map(ToString::to_string),
            "nameplateComponent": nameplate.and_then(|plate| plate.says.as_ref()).map_or(Value::Null, component),
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
        (Entity, &WorldName, &Position, &EntityKindComponent, Option<&MinecraftEntityId>, Option<&EntityDimensions>),
        (Option<&CustomName>, Option<&EntityUuid>, Has<Player>, Has<AbstractInsentient>),
        (Option<&Text>, Has<AbstractDisplay>, Option<&Invisible>, Option<&CustomNameVisible>),
        (Has<AbstractLiving>, Has<Dead>, Has<ArmorStand>, Option<&ArmorStandMarker>, Has<AbstractMinecart>, Has<AbstractBoat>),
        (Has<Interaction>, Option<&InteractionWidth>, Option<&InteractionHeight>),
        (Option<&ItemDisplayItemStack>, Option<&BlockDisplayBlockState>),
    ), Without<LocalEntity>>();

    let mut seen: Vec<Seen> = query
        .iter(&ecs)
        .filter(|((_, name, ..), ..)| **name == world)
        .map(|(common, named, floating, living, hitbox, (item, block))| {
            let (entity, _, position, kind, id, dimensions) = common;
            let (custom, uuid, player, mob) = named;
            let (text, display, invisible, name_visible) = floating;
            let (living, dead, stand, marker, minecart, boat) = living;
            let (interaction, width, height) = hitbox;

            let kind = plain(kind.0.to_str());
            let custom = custom.and_then(|name| name.0.as_deref().cloned());
            let label = match (&custom, uuid.and_then(|uuid| players.get(uuid))) {
                (Some(custom), _) => custom.to_string(),
                (None, Some(info)) if player => info.profile.name.clone(),
                _ => kind.to_owned(),
            };
            let (dx, dy, dz) = ((from.x - position.x) as f32, (from.y - position.y) as f32, (from.z - position.z) as f32);

            /* An armor stand is a hologram when it is only there to show its name. */
            let hologram = stand && invisible.is_some_and(|flag| flag.0) && name_visible.is_some_and(|flag| flag.0);
            let says = text.map(|text| (*text.0).clone()).or_else(|| custom.clone().filter(|_| hologram));

            /* An interaction entity's size is its own metadata, where azalea gives it none. */
            let bounds = match (interaction, width, height) {
                (true, Some(width), Some(height)) => EntityDimensions::new(width.0, height.0).make_bounding_box(**position),
                _ => dimensions.map_or_else(|| EntityDimensions::new(0.0, 0.0), Clone::clone).make_bounding_box(**position),
            };

            Seen {
                entity,
                id: id.map(|id| id.0),
                kind,
                label,
                custom,
                says,
                position: **position,
                bounds,
                distance: (dx * dx + dy * dy + dz * dz).sqrt(),
                player,
                mob,
                /* What the game lets a player's crosshair land on. */
                pickable: interaction
                    || minecart && !dead
                    || boat
                    || living && !dead && !marker.is_some_and(|marker| marker.0),
                display,
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

/// Whether feet at this offset from a label stand under it. The offset is the label's position
/// minus the feet, so a label above the feet has a positive y.
fn under(offset: Vec3) -> bool {
    offset.y >= 0.0 && offset.y <= BELOW && offset.x * offset.x + offset.z * offset.z <= ASIDE * ASIDE
}

/// The entity a label belongs to: of everything a player could click standing under it, the one
/// nearest the label. Displays and other labels are not clickable, whatever their bounds say.
fn owner(label: &Seen, entities: &[Seen]) -> Option<usize> {
    entities
        .iter()
        .enumerate()
        .filter(|(_, seen)| seen.entity != label.entity && !seen.display && seen.says.is_none() && seen.pickable)
        .filter(|(_, seen)| under(label.position - seen.position))
        .min_by(|(_, a), (_, b)| {
            a.position.distance_squared_to(label.position).total_cmp(&b.position.distance_squared_to(label.position))
        })
        .map(|(index, _)| index)
}

/// The label over each entity that has one, by index. A label belongs to one entity; an entity under
/// two, a name and a title stacked as separate displays, is labelled by the lower of them.
fn nameplates(entities: &[Seen]) -> HashMap<usize, usize> {
    let mut labels: HashMap<usize, usize> = HashMap::new();

    for (index, label) in entities.iter().enumerate() {
        if label.readable().is_none() {
            continue;
        }
        let Some(owner) = owner(label, entities) else {
            continue;
        };
        let at = entities[owner].position;
        let closer = labels
            .get(&owner)
            .is_none_or(|kept| label.position.distance_squared_to(at) < entities[*kept].position.distance_squared_to(at));
        if closer {
            labels.insert(owner, index);
        }
    }
    labels
}

/// Which entity interact-entity and attack-entity mean.
///
/// Exactly one, because two can name different entities and either choice would click something
/// the caller may not have meant. A refusal and not an argument error, since the arguments are the
/// shape the catalogue describes and the server would otherwise drop the tool as a mismatch.
enum Selector {
    Name(String),
    Label(String),
    Id(i32),
    Crosshair,
}

impl Selector {
    fn of(args: &Value) -> Result<Selector, Failure> {
        let name = args["name"].as_str().map(|name| Selector::Name(name.to_owned()));
        let label = args["label"].as_str().map(|label| Selector::Label(label.to_owned()));
        let id = match &args["id"] {
            Value::Null => None,
            _ => Some(Selector::Id(integer(args, "id", 0)? as i32)),
        };
        let crosshair = (args["crosshair"].as_bool() == Some(true)).then_some(Selector::Crosshair);

        let mut given: Vec<(&str, Selector)> =
            [("name", name), ("label", label), ("id", id), ("crosshair", crosshair)]
                .into_iter()
                .filter_map(|(word, selector)| selector.map(|selector| (word, selector)))
                .collect();

        if given.len() == 1 {
            return Ok(given.remove(0).1);
        }
        let words: Vec<&str> = given.iter().map(|(word, _)| *word).collect();
        let gave = match words.split_last() {
            None => "none".to_owned(),
            Some((last, [])) => (*last).to_owned(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        };
        Err(Failure::refused(
            "ONE_SELECTOR",
            format!("Say which entity with exactly one of name, label, id or crosshair; this call gave {gave}."),
        ))
    }

    /// The entity meant, and for the crosshair where on it the crosshair met it.
    fn resolve(&self, client: &Client, max_distance: f64) -> Result<(Seen, Option<Vec3>), Failure> {
        match self {
            Selector::Name(query) => require(client, query, max_distance).map(|seen| (seen, None)),
            Selector::Label(query) => under_label(client, query, max_distance).map(|seen| (seen, None)),
            Selector::Id(id) => nearby(client)
                .into_iter()
                .find(|seen| seen.id == Some(*id))
                .map(|seen| (seen, None))
                .ok_or_else(|| Failure::refused("NO_SUCH_ENTITY", format!("No entity with id {id} is in the bot's world."))),
            Selector::Crosshair => crosshair(client)
                .map(|(seen, at)| (seen, Some(at)))
                .ok_or_else(|| Failure::refused("NOT_LOOKING_AT_ENTITY", "The crosshair is not on an entity within reach.")),
        }
    }
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

/// The nearest label that reads like the query and has an entity in range under it.
fn under_label(client: &Client, query: &str, max_distance: f64) -> Result<Seen, Failure> {
    let mut entities = nearby(client);
    let needle = query.to_lowercase();

    let found = entities.iter().filter(|seen| seen.readable().is_some_and(|said| said.to_lowercase().contains(&needle))).find_map(|label| {
        owner(label, &entities).filter(|owner| f64::from(entities[*owner].distance) <= max_distance)
    });
    if let Some(owner) = found {
        return Ok(entities.swap_remove(owner));
    }

    let listed: Vec<String> = entities
        .iter()
        .filter(|seen| f64::from(seen.distance) <= max_distance)
        .filter_map(|seen| seen.readable().map(|said| format!("{said} ({} blocks away)", one_decimal(f64::from(seen.distance)))))
        .take(MAX_LISTED)
        .collect();
    let instead = if listed.is_empty() { "No label is in range either.".to_owned() } else { format!("Labels in range: {}.", listed.join(", ")) };

    Err(Failure::refused(
        "NO_SUCH_LABEL",
        format!(
            "Nothing within {} blocks stands under a label reading like \"{query}\". {instead}",
            super::text::plain(max_distance)
        ),
    ))
}

/// What the crosshair is on, and where on it: the game's own pick, done here.
///
/// azalea keeps a hit result of its own, but it gives an interaction entity no size, so the hitbox
/// a model is clicked through is never under its crosshair. The walk is the game's: blocks cut the
/// ray short, the nearest box the ray enters wins, and an entity beyond the entity reach is missed.
fn crosshair(client: &Client) -> Option<(Seen, Vec3)> {
    let eyes = client.position().up(f64::from(client.get_component::<EntityDimensions>()?.eye_height));
    let look = client.direction();
    let (entity_reach, block_reach) = {
        let attributes = client.get_component::<Attributes>()?;
        (attributes.entity_interaction_range.calculate(), attributes.block_interaction_range.calculate())
    };

    let reach = entity_reach.max(block_reach);
    let block = pick_block(look, eyes, &client.world().read().chunks, reach);
    let limit = if block.miss { reach } else { block.location.distance_to(eyes) };
    let end = eyes + view_vector(look) * limit;

    nearby(client)
        .into_iter()
        .filter(|seen| seen.pickable)
        .filter_map(|seen| {
            let at = if seen.bounds.contains(eyes) { Some(eyes) } else { seen.bounds.clip(eyes, end) }?;
            Some((at.distance_to(eyes), seen, at))
        })
        .filter(|(distance, ..)| *distance < entity_reach)
        .min_by(|(a, ..), (b, ..)| a.total_cmp(b))
        .map(|(_, seen, at)| (seen, at))
}

/// A tick before reading the crosshair. azalea applies a turn on its next update rather than when it
/// is asked for, so a look-at answered just before this call has not moved the crosshair yet, and
/// the click went to where the bot had been looking.
async fn settle(bot: &Bot, selector: &Selector) {
    if matches!(selector, Selector::Crosshair) {
        tick(bot).await;
    }
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
                /* From everything loaded: a label out of range can still be over an entity that is in it. */
                let seen = nearby(&game.client);
                let labels = nameplates(&seen);

                seen.iter()
                    .enumerate()
                    .filter(|(_, seen)| f64::from(seen.distance) <= max_distance)
                    .filter(|(_, seen)| query.as_deref().is_none_or(|query| seen.matches(query)))
                    .take(count)
                    .map(|(index, one)| one.describe(labels.get(&index).map(|label| &seen[*label])))
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
            let selector = Selector::of(&args)?;
            let max_distance = max_distance(&args, 8.0);
            let times = integer(&args, "times", 1)?;

            settle(&bot, &selector).await;
            let (target, _) = alive(&bot, |game| selector.resolve(&game.client, max_distance))??;
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

                if matches!(selector, Selector::Crosshair) {
                    /* Not followed: a target picked by where the bot looks is hit only while it is there. */
                    let still = in_world(&bot, |game| crosshair(&game.client))?.is_some_and(|(seen, _)| seen.entity == target.entity);
                    if !still {
                        return Ok(Answer::text(format!(
                            "Hit {label} {landed} time(s) out of {times}; it left the crosshair before the rest landed."
                        )));
                    }
                    alive(&bot, |game| attack(&game.client, target.entity, None))??;
                } else {
                    if !approach.reached(&bot, position, &label)? {
                        tick(&bot).await;
                        continue;
                    }
                    alive(&bot, |game| attack(&game.client, target.entity, Some(eyes)))??;
                }
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
            let selector = Selector::of(&args)?;
            let max_distance = max_distance(&args, 8.0);

            settle(&bot, &selector).await;
            let (target, aimed) = alive(&bot, |game| selector.resolve(&game.client, max_distance))??;

            if aimed.is_none() {
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
            }

            alive(&bot, |game| interact(&game.client, target.entity, aimed))??;
            Ok(Answer::text(format!("Right-clicked {}.", target.named())))
        })
    },
};

/// One attack packet, with the entity id written as the VarInt the game reads, after turning to
/// where it is given.
///
/// azalea's attack packet writes the id as a four-byte int. The server cannot decode that and drops
/// the connection, so a bot that swung at a mob was thrown out of the world. The packet's id still
/// comes from azalea's own table; only the body is written here.
///
/// Upstream: `ServerboundAttack::entity_id` in azalea-protocol 0.16.0 (`s_attack.rs`) lacks the
/// `#[var]` its `ServerboundInteract` has. Once azalea marks it, `Client::attack` does this.
pub(super) fn attack(client: &Client, entity: Entity, look: Option<Vec3>) -> Result<(), Failure> {
    let target = *client
        .get_entity_component::<MinecraftEntityId>(entity)
        .ok_or_else(|| Failure::refused("NO_SUCH_ENTITY", "the entity left the world before it was hit."))?;

    let mut packet = Vec::new();
    let _ = ServerboundAttack { entity_id: target }.into_variant().id().azalea_write_var(&mut packet);
    let _ = target.azalea_write_var(&mut packet);

    if let Some(look) = look {
        client.look_at(look);
    }
    client
        .with_raw_connection_mut(|mut connection| connection.net_conn().map(|network| network.write_raw(&packet).is_ok()))
        .filter(|written| *written)
        .ok_or_else(Failure::not_in_game)?;
    swing(client);
    Ok(())
}

/// One interact packet, as the game sends it: at the eyes after turning to them, or where the
/// crosshair met the entity without turning.
///
/// azalea's own sends the entity's position where the game sends the click relative to the entity,
/// which puts the click far outside anything an armour stand or a plugin reading the location would
/// accept, and it sends the packet twice, the way an older version did.
///
/// Upstream: `handle_entity_interact` in azalea-client 0.16.0 (`plugins/interact/mod.rs`). Once it
/// subtracts the entity's position and sends once, `Client::entity_interact` does this.
pub(super) fn interact(client: &Client, entity: Entity, aimed: Option<Vec3>) -> Result<(), Failure> {
    let lost = || Failure::refused("NO_SUCH_ENTITY", "the entity left the world before it was clicked.");
    let (position, eyes) = whereabouts(client, entity).ok_or_else(lost)?;
    let id = *client.get_entity_component::<MinecraftEntityId>(entity).ok_or_else(lost)?;

    let at = aimed.unwrap_or_else(|| {
        client.look_at(eyes);
        eyes
    });
    client.write_packet(ServerboundInteract {
        entity_id: id,
        hand: InteractionHand::MainHand,
        location: LpVec3::from(at - position),
        using_secondary_action: client.crouching(),
    });
    swing(client);
    Ok(())
}

#[cfg(test)]
mod tests {
    use azalea::Vec3;

    use super::under;

    /// The offset is the label minus the feet, and getting its sign backwards reads a label as
    /// belonging to whatever stands on top of it rather than to the NPC it floats over.
    #[test]
    fn feet_up_to_three_blocks_below_and_one_to_the_side_are_under() {
        assert!(under(Vec3::new(0.0, 2.4, 0.0)));
        assert!(under(Vec3::new(0.0, 3.0, 0.0)));
        assert!(under(Vec3::new(0.6, 0.0, 0.8)));
    }

    #[test]
    fn feet_above_the_label_or_further_off_are_not() {
        assert!(!under(Vec3::new(0.0, -0.1, 0.0)));
        assert!(!under(Vec3::new(0.0, 3.1, 0.0)));
        assert!(!under(Vec3::new(0.8, 2.0, 0.8)));
    }
}
