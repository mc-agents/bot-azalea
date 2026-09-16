//! What a vanilla client keeps about the menus it draws, from packets azalea decodes and drops.
//!
//! A trading screen's offers, the numbers an enchanting table, a beacon or a lectern pushes into its
//! menu, the stonecutter's recipe list, the recipe book, an open book, and the tags and registry
//! entries those are named from: azalea reads every one of these off the wire and keeps none. Each
//! is kept here on the player's entity, by systems inside the ECS, so a tool reads it the way it
//! reads the inventory.

use std::collections::HashMap;

use azalea::Identifier;
use azalea::app::{App, Plugin, Update};
use azalea::ecs::prelude::*;
use azalea::packet::config::ReceiveConfigPacketEvent;
use azalea::packet::game::ReceiveGamePacketEvent;
use azalea::protocol::common::recipe::SlotDisplayData;
use azalea::protocol::packets::config::ClientboundConfigPacket;
use azalea::protocol::packets::game::ClientboundGamePacket;
use azalea::protocol::packets::game::c_merchant_offers::ClientboundMerchantOffers;
use azalea::protocol::packets::game::c_recipe_book_add::RecipeDisplayEntry;
use azalea::protocol::packets::game::c_update_recipes::SingleInputEntry;
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::protocol::common::tags::TagMap;
use simdnbt::owned::NbtCompound;

pub struct MenusPlugin;

impl Plugin for MenusPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (keep_config, keep_game).chain());
    }
}

/// The registries whose entries are read by name: what an enchantment is called and how high it
/// goes, and what a banner pattern is called. The rest are not kept.
const KEPT_REGISTRIES: &[&str] = &["minecraft:enchantment", "minecraft:banner_pattern"];

/// State that belongs to the menu on screen, and to the recipe book.
#[derive(Component, Default)]
pub struct Menus {
    /// The window the data values below belong to. A window opening starts a new set: the values
    /// of the last one are not this one's.
    pub data_for: i32,
    /// The menu's data values, as the signed shorts the game sends them as. An enchantment clue of
    /// -1 arrives as 65535 in azalea's unsigned field.
    pub data: HashMap<u16, i16>,
    pub offers: Option<ClientboundMerchantOffers>,
    pub stonecutter: Vec<SingleInputEntry>,
    /// The recipe book, in the order the server taught it.
    pub recipes: Vec<RecipeDisplayEntry>,
    /// A book being read from the hand. A lectern is a menu and is told apart by its window.
    pub book: Option<InteractionHand>,
}

impl Menus {
    pub fn value(&self, window: i32, index: u16) -> Option<i32> {
        if self.data_for != window {
            return None;
        }
        self.data.get(&index).copied().map(i32::from)
    }
}

/// What the server said while configuring the connection: the tags, and the registry entries a
/// menu is named from.
#[derive(Clone, Component, Default)]
pub struct Known {
    /// Registry, then tag, then the protocol ids of its members in the order the server sent them.
    pub tags: HashMap<Identifier, HashMap<Identifier, Vec<i32>>>,
    pub registries: HashMap<Identifier, Vec<(Identifier, Option<NbtCompound>)>>,
}

impl Known {
    pub fn tag(&self, registry: &str, tag: &Identifier) -> &[i32] {
        self.tags
            .get(&Identifier::new(registry))
            .and_then(|tags| tags.get(tag))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn entry(&self, registry: &str, id: usize) -> Option<&(Identifier, Option<NbtCompound>)> {
        self.registries.get(&Identifier::new(registry)).and_then(|entries| entries.get(id))
    }

    fn replace_tags(&mut self, map: &TagMap) {
        for (registry, tags) in &map.0 {
            let kept = tags.iter().map(|tag| (tag.name.clone(), tag.elements.clone())).collect();
            self.tags.insert(registry.clone(), kept);
        }
    }
}

fn keep_game(mut received: MessageReader<ReceiveGamePacketEvent>, mut players: Query<(&mut Menus, &mut Known)>) {
    use ClientboundGamePacket as P;

    for ReceiveGamePacketEvent { entity, packet } in received.read() {
        let Ok((mut menus, mut known)) = players.get_mut(*entity) else {
            continue;
        };
        match packet.as_ref() {
            P::OpenScreen(p) => {
                menus.data_for = p.container_id;
                menus.data.clear();
                menus.offers = None;
                menus.book = None;
            }
            P::ContainerSetData(p) => {
                if menus.data_for != p.container_id {
                    menus.data_for = p.container_id;
                    menus.data.clear();
                }
                menus.data.insert(p.id, p.value as i16);
            }
            P::MerchantOffers(p) => menus.offers = Some(p.clone()),
            P::OpenBook(p) => menus.book = Some(p.hand),
            /* A new level puts the client's loading screen up in place of whatever was open, a book included. */
            P::Login(_) | P::Respawn(_) => menus.book = None,
            P::UpdateRecipes(p) => menus.stonecutter = p.stonecutter_recipes.clone(),
            P::RecipeBookAdd(p) => {
                if p.replace {
                    menus.recipes.clear();
                }
                for entry in &p.entries {
                    menus.recipes.retain(|kept| kept.id != entry.contents.id);
                    menus.recipes.push(entry.contents.clone());
                }
            }
            P::RecipeBookRemove(p) => menus.recipes.retain(|kept| !p.recipes.contains(&kept.id)),
            P::UpdateTags(p) => known.replace_tags(&p.tags),
            _ => {}
        }
    }
}

fn keep_config(mut received: MessageReader<ReceiveConfigPacketEvent>, mut players: Query<&mut Known>) {
    for ReceiveConfigPacketEvent { entity, packet } in received.read() {
        let Ok(mut known) = players.get_mut(*entity) else {
            continue;
        };
        match packet.as_ref() {
            ClientboundConfigPacket::UpdateTags(p) => known.replace_tags(&p.tags),
            ClientboundConfigPacket::RegistryData(p) if KEPT_REGISTRIES.contains(&p.registry_id.to_string().as_str()) => {
                known.registries.insert(p.registry_id.clone(), p.entries.clone());
            }
            _ => {}
        }
    }
}

/// The first stack a slot display stands for, the way the game resolves one to draw a button or
/// name an ingredient. `None` is a display with nothing to show, or one that needs the world to
/// work out -- a dyed or trimmed demonstration -- which nothing here reads.
pub fn first_item(display: &SlotDisplayData, known: &Known) -> Option<(azalea::registry::builtin::ItemKind, i32)> {
    use azalea::registry::Registry;
    use azalea::registry::builtin::ItemKind;

    match display {
        SlotDisplayData::Item(item) => Some((item.item, 1)),
        SlotDisplayData::ItemStack(stack) => Some((stack.stack.item, stack.stack.count)),
        SlotDisplayData::Tag(tag) => known
            .tag("minecraft:item", &tag.tag)
            .first()
            .and_then(|id| ItemKind::from_u32(*id as u32))
            .map(|item| (item, 1)),
        SlotDisplayData::WithRemainder(remainder) => first_item(&remainder.input, known),
        SlotDisplayData::Composite(composite) => composite.contents.iter().find_map(|display| first_item(display, known)),
        _ => None,
    }
}
