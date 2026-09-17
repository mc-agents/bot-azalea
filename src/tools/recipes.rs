//! What the bot has been taught to craft, and crafting it.
//!
//! A client is sent a recipe as the server unlocks it and is never told the whole set, so all of
//! this is about what the bot can place rather than about what the game can make, and every answer
//! says so with `onlyWhatTheBotKnows`: "no recipe" from a client means "not taught to me".

use std::collections::HashMap;
use std::str::FromStr;

use azalea::container::ContainerHandleRef;
use azalea::core::direction::Direction;
use azalea::entity::inventory::Inventory;
use azalea::inventory::components::{CustomName, Damage, Enchantments};
use azalea::inventory::operations::ClickType;
use azalea::inventory::{ItemStack, Menu};
use azalea::protocol::common::recipe::{Ingredient, RecipeDisplayData, SlotDisplayData};
use azalea::protocol::packets::game::c_recipe_book_add::RecipeDisplayEntry;
use azalea::protocol::packets::game::s_place_recipe::ServerboundPlaceRecipe;
use azalea::registry::builtin::{BlockKind, ItemKind};
use azalea::registry::{HolderSet, Registry};
use azalea::{BlockPos, Client};
use serde_json::{Value, json};

use super::args::{integer, plain, text};
use super::hands::{approach, use_item_on};
use super::inventory::player_menu;
use super::windows::{click, menu};
use super::{Tool, alive, in_world, tick};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};
use crate::menus::{Known, Menus, first_item};

/// A player carries a two-by-two; anything larger is a crafting table.
const PLAYER_GRID: u32 = 2;

/// How far list-recipes and can-craft look for a table, and how far craft-item walks to one: the
/// distances the other kinds of bot use, so all of them give up at the same place.
const TABLE_IN_REACH: i32 = 8;
const TABLE_SEARCH: i32 = 16;

/// The same cap the other kinds of bot apply, so a capped answer is capped at the same place.
const MAX_LISTED: usize = 100;

const RESULT_SLOT: usize = 0;
const BAG_START: usize = 9;

/// The server fills the grid when it gets round to it; a few ticks is plenty.
const PATIENCE_TICKS: u32 = 40;

/// The recipe book and the tags its displays name, taken out of the ECS for one answer.
struct Book {
    recipes: Vec<RecipeDisplayEntry>,
    known: Known,
}

fn book(client: &Client) -> Book {
    let recipes = client
        .get_component::<Menus>()
        .map(|menus| menus.recipes.clone())
        .unwrap_or_default();
    let known = client
        .get_component::<Known>()
        .map(|known| known.clone())
        .unwrap_or_default();
    Book { recipes, known }
}

fn is_crafting(display: &RecipeDisplayData) -> bool {
    matches!(display, RecipeDisplayData::Shaped(_) | RecipeDisplayData::Shapeless(_))
}

/// Whether a recipe wants more room than the player's own grid. Placing one there anyway is refused
/// by the server without a word, which would read as a result that never appeared.
fn requires_table(display: &RecipeDisplayData) -> bool {
    match display {
        RecipeDisplayData::Shaped(shaped) => shaped.width > PLAYER_GRID || shaped.height > PLAYER_GRID,
        RecipeDisplayData::Shapeless(shapeless) => shapeless.ingredients.len() > (PLAYER_GRID * PLAYER_GRID) as usize,
        _ => true,
    }
}

fn result(display: &RecipeDisplayData) -> &SlotDisplayData {
    match display {
        RecipeDisplayData::Shaped(shaped) => &shaped.result,
        RecipeDisplayData::Shapeless(shapeless) => &shapeless.result,
        RecipeDisplayData::Furnace(furnace) => &furnace.result,
        RecipeDisplayData::Stonecutter(stonecutter) => &stonecutter.result,
        RecipeDisplayData::Smithing(smithing) => &smithing.result,
    }
}

/// Every stack a display stands for, as the game resolves one to list a recipe's results.
fn stacks_of(display: &SlotDisplayData, known: &Known) -> Vec<(ItemKind, i32)> {
    match display {
        SlotDisplayData::Tag(tag) => known
            .tag("minecraft:item", &tag.tag)
            .iter()
            .filter_map(|id| ItemKind::from_u32(*id as u32))
            .map(|item| (item, 1))
            .collect(),
        SlotDisplayData::Composite(composite) => composite
            .contents
            .iter()
            .flat_map(|display| stacks_of(display, known))
            .collect(),
        other => first_item(other, known).into_iter().collect(),
    }
}

fn path(item: ItemKind) -> &'static str {
    plain(item.to_str())
}

impl Book {
    /// Every taught crafting recipe that makes the named item. A furnace is smelt-item's.
    fn producing(&self, item: &str) -> Vec<&RecipeDisplayEntry> {
        self.recipes
            .iter()
            .filter(|entry| is_crafting(&entry.display))
            .filter(|entry| {
                stacks_of(result(&entry.display), &self.known)
                    .iter()
                    .any(|(kind, _)| path(*kind) == item)
            })
            .collect()
    }

    /// One line per distinct ingredient, with how many of it the recipe takes. A slot display is a
    /// set of things that would do -- any plank -- and the first is named, because a caller wants a
    /// name to go and get rather than the whole set.
    fn ingredients(&self, entry: &RecipeDisplayEntry) -> Vec<(&'static str, i32)> {
        let slots = match &entry.display {
            RecipeDisplayData::Shaped(shaped) => &shaped.ingredients,
            RecipeDisplayData::Shapeless(shapeless) => &shapeless.ingredients,
            _ => return Vec::new(),
        };
        let mut counted: Vec<(&'static str, i32)> = Vec::new();
        for (item, _) in slots.iter().filter_map(|slot| first_item(slot, &self.known)) {
            match counted.iter_mut().find(|(name, _)| *name == path(item)) {
                Some((_, count)) => *count += 1,
                None => counted.push((path(item), 1)),
            }
        }
        counted
    }

    fn describe(&self, entry: &RecipeDisplayEntry, held: &HashMap<&'static str, i32>) -> Value {
        let needed = self.ingredients(entry);
        let (item, count) = stacks_of(result(&entry.display), &self.known)
            .first()
            .copied()
            .unwrap_or((ItemKind::Air, 0));
        json!({
            "result": {"name": path(item), "count": count},
            "ingredients": counts(&needed),
            "missing": counts(&missing(held, &needed)),
            "requiresTable": requires_table(&entry.display),
        })
    }

    fn accepts(&self, ingredient: &Ingredient, item: ItemKind) -> bool {
        match &ingredient.allowed {
            HolderSet::Direct { contents } => contents.contains(&item),
            HolderSet::Named { key, .. } => self.known.tag("minecraft:item", key).contains(&(item.to_u32() as i32)),
        }
    }

    /// Whether the inventory holds one of everything the recipe asks for, each ingredient served by
    /// its own item. The ingredients that accept the fewest items are served first, so an item one
    /// of them needs is not spent on another that would have taken something else.
    fn can_craft(&self, entry: &RecipeDisplayEntry, simple: &HashMap<ItemKind, i32>) -> bool {
        let Some(requirements) = &entry.crafting_requirements else {
            return false;
        };
        let mut left = simple.clone();
        let mut ordered: Vec<&Ingredient> = requirements.iter().collect();
        ordered.sort_by_key(|ingredient| left.keys().filter(|item| self.accepts(ingredient, **item)).count());

        ordered.iter().all(|ingredient| {
            let found = left
                .iter_mut()
                .find(|(item, count)| **count > 0 && self.accepts(ingredient, **item));
            match found {
                Some((_, count)) => {
                    *count -= 1;
                    true
                }
                None => false,
            }
        })
    }
}

fn counts(counted: &[(&'static str, i32)]) -> Vec<Value> {
    counted
        .iter()
        .map(|(name, count)| json!({"name": name, "count": count}))
        .collect()
}

/// What the inventory is short of for one recipe. Empty means it can be made now.
fn missing(held: &HashMap<&'static str, i32>, needed: &[(&'static str, i32)]) -> Vec<(&'static str, i32)> {
    needed
        .iter()
        .filter_map(|(name, count)| {
            let gap = count - held.get(name).copied().unwrap_or(0);
            (gap > 0).then_some((*name, gap))
        })
        .collect()
}

/// What the bot carries, by item: in the bag and on the hotbar, as a recipe counts it.
fn held(client: &Client) -> HashMap<&'static str, i32> {
    let mut held = HashMap::new();
    if let Some(inventory) = client.get_component::<Inventory>() {
        for stack in player_menu(&inventory)
            .slots()
            .iter()
            .skip(BAG_START)
            .filter(|stack| !stack.is_empty())
        {
            *held.entry(path(stack.kind())).or_insert(0) += stack.count();
        }
    }
    held
}

/// The stacks a recipe may take: the game passes over anything damaged, enchanted or named.
fn simple(client: &Client) -> HashMap<ItemKind, i32> {
    let mut simple = HashMap::new();
    if let Some(inventory) = client.get_component::<Inventory>() {
        for stack in player_menu(&inventory).slots() {
            let plain = stack.get_component::<CustomName>().is_none()
                && stack
                    .get_component::<Enchantments>()
                    .is_none_or(|enchantments| enchantments.levels.is_empty())
                && stack.get_component::<Damage>().is_none_or(|damage| damage.amount == 0);
            if !stack.is_empty() && plain {
                *simple.entry(stack.kind()).or_insert(0) += stack.count();
            }
        }
    }
    simple
}

/// The nearest crafting table within a cube around the bot's feet.
fn nearest_table(client: &Client, radius: i32) -> Option<BlockPos> {
    let feet = BlockPos::from(client.position());
    let world = client.world();
    let world = world.read();
    let mut nearest: Option<(i32, BlockPos)> = None;

    for x in -radius..=radius {
        for y in -radius..=radius {
            for z in -radius..=radius {
                let at = BlockPos::new(feet.x + x, feet.y + y, feet.z + z);
                if world
                    .get_block_state(at)
                    .is_some_and(|state| BlockKind::from(state) == BlockKind::CraftingTable)
                {
                    let distance = x * x + y * y + z * z;
                    if nearest.is_none_or(|(best, _)| distance < best) {
                        nearest = Some((distance, at));
                    }
                }
            }
        }
    }
    nearest.map(|(_, at)| at)
}

/// Fewest missing ingredients first, which is what a caller is told to go and get.
fn by_shortfall<'a>(
    book: &Book,
    held: &HashMap<&'static str, i32>,
    mut entries: Vec<&'a RecipeDisplayEntry>,
) -> Vec<&'a RecipeDisplayEntry> {
    entries.sort_by_key(|entry| missing(held, &book.ingredients(entry)).len());
    entries
}

pub const CAN_CRAFT: Tool = Tool {
    name: "can-craft",
    run: |bot, args| {
        Box::pin(async move {
            let item = super::stacks::needle(text(&args, "itemName")?);

            let data = alive(&bot, |game| {
                let (book, held) = (book(&game.client), held(&game.client));
                let recipes = by_shortfall(&book, &held, book.producing(&item));
                let Some(closest) = recipes.first() else {
                    return json!({
                        "item": item, "onlyWhatTheBotKnows": true, "craftable": false,
                        "hasRecipe": false, "missing": [], "needsTable": false,
                    });
                };
                let missing = missing(&held, &book.ingredients(closest));
                let needs_table =
                    requires_table(&closest.display) && nearest_table(&game.client, TABLE_IN_REACH).is_none();
                json!({
                    "item": item,
                    "onlyWhatTheBotKnows": true,
                    "craftable": missing.is_empty() && !needs_table,
                    "hasRecipe": true,
                    "missing": counts(&missing),
                    "needsTable": needs_table,
                })
            })?;
            Ok(Answer::data("can-craft", data))
        })
    },
};

/// Every taught recipe for one item, with what the inventory is still short of. `stoppedAt` is
/// always null: only list-recipes' unrestricted scan can run out of room.
pub const GET_RECIPE: Tool = Tool {
    name: "get-recipe",
    run: |bot, args| {
        Box::pin(async move {
            let item = super::stacks::needle(text(&args, "itemName")?);

            let data = alive(&bot, |game| {
                let (book, held) = (book(&game.client), held(&game.client));
                let recipes: Vec<Value> = by_shortfall(&book, &held, book.producing(&item))
                    .iter()
                    .map(|entry| book.describe(entry, &held))
                    .collect();
                json!({
                    "item": item,
                    "tableInReach": nearest_table(&game.client, TABLE_IN_REACH).is_some(),
                    "stoppedAt": null,
                    "recipes": recipes,
                    "onlyWhatTheBotKnows": true,
                })
            })?;
            Ok(Answer::data("get-recipe", data))
        })
    },
};

/// What the bot could make right now, or every taught recipe for one item.
pub const LIST_RECIPES: Tool = Tool {
    name: "list-recipes",
    run: |bot, args| {
        Box::pin(async move {
            let item = match &args["outputItem"] {
                Value::Null => None,
                _ => Some(super::stacks::needle(text(&args, "outputItem")?)),
            };

            let data = alive(&bot, |game| {
                let (book, held) = (book(&game.client), held(&game.client));
                let table_in_reach = nearest_table(&game.client, TABLE_IN_REACH).is_some();

                if let Some(item) = &item {
                    let recipes: Vec<Value> = by_shortfall(&book, &held, book.producing(item))
                        .iter()
                        .map(|entry| book.describe(entry, &held))
                        .collect();
                    return json!({
                        "tableInReach": table_in_reach, "onlyWhatTheBotKnows": true, "item": item,
                        "stoppedAt": null, "recipes": recipes,
                    });
                }

                /* Craftable right now means right now: a recipe that needs a table out of reach is not one. */
                let craftable: Vec<Value> = book
                    .recipes
                    .iter()
                    .filter(|entry| is_crafting(&entry.display))
                    .filter(|entry| !requires_table(&entry.display) || table_in_reach)
                    .filter(|entry| missing(&held, &book.ingredients(entry)).is_empty())
                    .take(MAX_LISTED)
                    .map(|entry| book.describe(entry, &held))
                    .collect();
                json!({
                    "tableInReach": table_in_reach,
                    "onlyWhatTheBotKnows": true,
                    "item": null,
                    "stoppedAt": if craftable.len() >= MAX_LISTED { json!(MAX_LISTED) } else { Value::Null },
                    "recipes": craftable,
                })
            })?;
            Ok(Answer::data("list-recipes", data))
        })
    },
};

/// Craft something, on a nearby table when there is one and in the player's own grid otherwise.
///
/// The server does the placing: the packet names the recipe and the server fills the grid out of the
/// inventory. One craft at a time rather than a shift-click on the result, because a shift-click
/// makes as many as the ingredients allow and the caller asked for a number.
pub const CRAFT_ITEM: Tool = Tool {
    name: "craft-item",
    run: |bot, args| {
        Box::pin(async move {
            let asked = text(&args, "outputItem")?.to_owned();
            let amount = integer(&args, "amount", 1)?;

            let output = item(&asked).ok_or_else(|| {
                Failure::refused("NO_SUCH_ITEM", format!("\"{asked}\" is not an item in this version."))
            })?;
            let name = path(output);

            let (recipe, before, table) = alive(&bot, |game| {
                let book = book(&game.client);
                let recipe = known(&book, name).ok_or_else(|| {
                    Failure::refused(
                        "NO_RECIPE",
                        format!("No recipe produces {name}, or the server has not unlocked one for this bot. A client is only told the recipes its book holds."),
                    )
                })?;
                if !book.can_craft(&recipe, &simple(&game.client)) {
                    return Err(Failure::refused(
                        "MISSING_INGREDIENTS",
                        format!(
                            "Cannot craft {name}. The recipe is known but the ingredients are not all in the inventory."
                        ),
                    ));
                }
                let table = nearest_table(&game.client, TABLE_SEARCH);
                if table.is_none() && requires_table(&recipe.display) {
                    return Err(Failure::refused(
                        "NO_CRAFTING_TABLE",
                        format!(
                            "Crafting {name} needs a grid bigger than the two-by-two a player carries, and no crafting table is within {TABLE_SEARCH} blocks."
                        ),
                    ));
                }
                Ok((recipe, count(&game.client, output), table))
            })??;

            let mut open_table = TableWindow {
                bot: &bot,
                window: None,
            };
            let window = match table {
                None => 0,
                Some(table) => {
                    approach(&bot, table).await?;
                    alive(&bot, |game| {
                        use_item_on(&game.client, table, Direction::Up, table.center())
                    })?;
                    let window = loop {
                        tick(&bot).await;
                        if let Some(window) = in_world(&bot, |game| {
                            menu(&game.client).filter(|window| matches!(window.menu, Menu::Crafting { .. }))
                        })? {
                            break window.id;
                        }
                    };
                    open_table.window = Some(window);
                    window
                }
            };

            for made in 0..amount {
                alive(&bot, |game| place(&game.client, window, recipe.id))?;

                let mut waited = 0;
                while in_world(&bot, |game| result_empty(&game.client, window))? {
                    waited += 1;
                    if waited > PATIENCE_TICKS {
                        let before = if made == 0 {
                            String::new()
                        } else {
                            format!(
                                " {} were made before that.",
                                made as i32 * per_craft(&bot, &recipe, output)
                            )
                        };
                        return Err(Failure::refused(
                            "NOTHING_CRAFTED",
                            format!("The server placed {name}'s recipe but no result appeared.{before}"),
                        ));
                    }
                    tick(&bot).await;
                }
                click(&bot, window, RESULT_SLOT as i16, 0, ClickType::QuickMove).await?;
            }
            drop(open_table);

            /*
            Counted rather than predicted: what the inventory actually gained cannot claim a craft that
            did not happen, which is a thing another kind of bot has done.
            */
            for _ in 0..=PATIENCE_TICKS {
                let gained = in_world(&bot, |game| count(&game.client, output))? - before;
                if gained > 0 {
                    return Ok(Answer::text(format!("Crafted {name} x{gained}.")));
                }
                tick(&bot).await;
            }
            Err(Failure::refused(
                "NOTHING_CRAFTED",
                format!(
                    "Nothing was crafted. The recipe for {name} was found and placed, but the inventory did not gain any."
                ),
            ))
        })
    },
};

/// The table's window, closed when the craft is over or when its call is dropped part way.
struct TableWindow<'a> {
    bot: &'a Bot,
    window: Option<i32>,
}

impl Drop for TableWindow<'_> {
    fn drop(&mut self) {
        if let Some(window) = self.window.take() {
            let _ = in_world(self.bot, |game| {
                if menu(&game.client).is_some_and(|open| open.id == window) {
                    ContainerHandleRef::new(window, game.client.clone()).close();
                }
            });
        }
    }
}

fn item(name: &str) -> Option<ItemKind> {
    let id = name.trim().to_lowercase();
    let path = match id.split_once(':') {
        Some(("minecraft", path)) => path.to_owned(),
        Some(_) => return None,
        None => id,
    };
    ItemKind::from_str(&path).ok()
}

/// The recipe the book holds for this item, preferring one that fits the player's own grid.
fn known(book: &Book, item: &str) -> Option<RecipeDisplayEntry> {
    let producing = book.producing(item);
    producing
        .iter()
        .find(|entry| !requires_table(&entry.display))
        .or(producing.first())
        .map(|entry| (*entry).clone())
}

fn per_craft(bot: &Bot, recipe: &RecipeDisplayEntry, output: ItemKind) -> i32 {
    in_world(bot, |game| {
        let book = book(&game.client);
        stacks_of(result(&recipe.display), &book.known)
            .iter()
            .find(|(item, _)| *item == output)
            .map_or(1, |(_, count)| *count)
    })
    .unwrap_or(1)
}

/// How many of the item are in the bag and on the hotbar.
fn count(client: &Client, item: ItemKind) -> i32 {
    client.get_component::<Inventory>().map_or(0, |inventory| {
        player_menu(&inventory)
            .slots()
            .iter()
            .skip(BAG_START)
            .filter(|stack| stack.kind() == item)
            .map(ItemStack::count)
            .sum()
    })
}

fn result_empty(client: &Client, window: i32) -> bool {
    client.get_component::<Inventory>().is_none_or(|inventory| {
        inventory.id != window || inventory.menu().slot(RESULT_SLOT).is_none_or(ItemStack::is_empty)
    })
}

/// Ask the server to lay a recipe into the grid, by the id the recipe book gave it.
fn place(client: &Client, window: i32, recipe: u32) {
    client.write_packet(ServerboundPlaceRecipe {
        container_id: window,
        recipe,
        shift_down: false,
    });
}
