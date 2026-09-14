use std::str::FromStr;

use azalea::entity::PlayerAbilities;
use azalea::entity::inventory::Inventory;
use azalea::inventory::operations::ClickType;
use azalea::inventory::{ItemStack, Menu};
use azalea::local_player::LocalGameMode;
use azalea::protocol::packets::game::s_set_creative_mode_slot::ServerboundSetCreativeModeSlot;
use azalea::registry::builtin::ItemKind;
use serde_json::{Value, json};

use super::args::{integer, text};
use super::windows::{OFF_HAND, click};
use super::{Tool, alive, game_mode, in_world, stacks};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};

/// The player's inventory window, as the server numbers it: 5-8 worn, 9-35 the bag, 36-44 the
/// hotbar, 45 the off-hand.
const WORN_HEAD: usize = 5;
const BAG_START: usize = 9;
const HOTBAR_START: usize = 36;
const HOTBAR_END: usize = 44;

/// The window id of the player's own inventory, which the server uses when nothing else is open.
const INVENTORY_WINDOW: i32 = 0;

/// The player's inventory window, numbered the way click-slot numbers it.
///
/// azalea keeps a second copy of the carried slots inside an open container and copies it back
/// only when the container closes, so while one is open the inventory window's own copy is the one
/// from before it opened. The container's is the current one.
fn player_menu(inventory: &Inventory) -> Menu {
    let mut menu = inventory.inventory_menu.clone();
    if let Some(container) = &inventory.container_menu {
        let from = *container.player_slots_range().start();
        for offset in 0..36 {
            if let (Some(slot), Some(current)) = (menu.slot_mut(BAG_START + offset), container.slot(from + offset)) {
                *slot = current.clone();
            }
        }
    }
    menu
}

pub const LIST_INVENTORY: Tool = Tool {
    name: "list-inventory",
    run: |bot, _args| {
        Box::pin(async move {
            /* Reading goes on behind the death screen, because what a death did is read after it. */
            let items = in_world(&bot, |game| {
                let menu = player_menu(&game.client.component::<Inventory>());
                menu.slots()
                    .iter()
                    .enumerate()
                    .filter(|(_, stack)| !stack.is_empty())
                    .map(|(slot, stack)| stacks::carried(stack, slot))
                    .collect::<Vec<_>>()
            })?;
            Ok(Answer::data("list-inventory", json!({"items": items})))
        })
    },
};

pub const FIND_ITEM: Tool = Tool {
    name: "find-item",
    run: |bot, args| {
        Box::pin(async move {
            let query = text(&args, "nameOrType")?.to_owned();

            /* Not found is a state, not a failure: a check often wants to know a thing is absent. */
            let item = in_world(&bot, |game| {
                let menu = player_menu(&game.client.component::<Inventory>());
                menu.slots()
                    .iter()
                    .enumerate()
                    .find(|(_, stack)| !stack.is_empty() && stacks::matches(stack, &query))
                    .map(|(slot, stack)| stacks::carried(stack, slot))
            })?;
            Ok(Answer::data("find-item", json!({"query": query, "item": item})))
        })
    },
};

/// Put an item where it can be used: in hand, in the off-hand, or worn.
///
/// All of it is clicks in the player's inventory window, the same a person makes, so the server
/// moves the item and nothing here writes the inventory behind its back. Armour is two picks rather
/// than a shift-click, because a shift-click sends whatever is not armour somewhere else entirely and
/// still looks like it worked.
pub const EQUIP_ITEM: Tool = Tool {
    name: "equip-item",
    run: |bot, args| {
        Box::pin(async move {
            let query = text(&args, "itemName")?.to_owned();
            let destination = text(&args, "destination")?.to_owned();

            let (source, item, selected) = alive(&bot, |game| {
                let inventory = game.client.component::<Inventory>();
                let menu = player_menu(&inventory);
                let found = menu
                    .slots()
                    .iter()
                    .enumerate()
                    .skip(BAG_START)
                    .find(|(_, stack)| !stack.is_empty() && stacks::matches(stack, &query))
                    .map(|(slot, stack)| (slot, stacks::name(stack), inventory.selected_hotbar_slot));
                found.ok_or_else(|| Failure::refused("NO_SUCH_ITEM", format!("No inventory item matches \"{query}\"")))
            })??;

            match destination.as_str() {
                /* Already on the hotbar is a held-item change, which is what a person does with a number key. */
                "hand" if (HOTBAR_START..=HOTBAR_END).contains(&source) => {
                    alive(&bot, |game| game.client.set_selected_hotbar_slot((source - HOTBAR_START) as u8))?;
                }
                "hand" => click(&bot, INVENTORY_WINDOW, source as i16, selected, ClickType::Swap).await?,
                "off-hand" => click(&bot, INVENTORY_WINDOW, source as i16, OFF_HAND, ClickType::Swap).await?,
                "head" | "torso" | "legs" | "feet" => {
                    let worn = WORN_HEAD + ["head", "torso", "legs", "feet"].iter().position(|part| *part == destination).unwrap_or(3);
                    wear(&bot, source, worn).await?;
                }
                other => return Err(Failure::bad_args(format!("unknown destination {other}"))),
            }

            Ok(Answer::text(format!("Equipped {item} to {destination}.")))
        })
    },
};

/// Pick it up, put it on, and put whatever came off back where the first one was. A stack left on
/// the cursor drops the moment anything else opens a window.
async fn wear(bot: &Bot, source: usize, worn: usize) -> Result<(), Failure> {
    if source == worn {
        return Ok(());
    }
    click(bot, INVENTORY_WINDOW, source as i16, 0, ClickType::Pickup).await?;
    click(bot, INVENTORY_WINDOW, worn as i16, 0, ClickType::Pickup).await?;

    if in_world(bot, |game| game.client.component::<Inventory>().carried.is_present())? {
        click(bot, INVENTORY_WINDOW, source as i16, 0, ClickType::Pickup).await?;
    }
    Ok(())
}

/// Put an item straight into the inventory, so a check starts from the state it needs rather than
/// gathering its way there.
///
/// Creative only, because the packet behind it is the creative inventory's and a server in any other
/// mode drops it. Saying so is better than a silent no-op.
pub const GIVE_ITEM: Tool = Tool {
    name: "give-item",
    run: |bot, args| {
        Box::pin(async move {
            let item_name = text(&args, "itemName")?.to_owned();
            let count = integer(&args, "count", 1)?;

            let slot = alive(&bot, |game| {
                let client = &game.client;
                let creative = client.get_component::<PlayerAbilities>().is_some_and(|abilities| abilities.instant_break);
                if !creative {
                    let mode = game_mode(client.get_component::<LocalGameMode>().map(|mode| mode.current));
                    return Err(Failure::refused("NOT_CREATIVE", format!("The bot is in {mode} mode; give-item needs creative.")));
                }

                let kind = item(&item_name)
                    .ok_or_else(|| Failure::refused("NO_SUCH_ITEM", format!("\"{item_name}\" is not an item in this version.")))?;

                let menu = player_menu(&client.component::<Inventory>());
                let slot = match &args["slot"] {
                    Value::Null => first_empty(&menu)?,
                    _ => integer(&args, "slot", -1)? as usize,
                };
                let stack = ItemStack::new(kind, count as i32);

                /*
                The client has to put it there itself. A creative inventory change is the one place the
                server trusts the client and sends nothing back, so sending only the packet left the
                server holding a stack the bot could not see.
                */
                let _ = client.try_query_self::<&mut Inventory, _>(|mut inventory| {
                    if let Some(own) = inventory.inventory_menu.slot_mut(slot) {
                        *own = stack.clone();
                    }
                    if let Some(container) = inventory.container_menu.as_mut()
                        && (BAG_START..=HOTBAR_END).contains(&slot)
                    {
                        let at = *container.player_slots_range().start() + slot - BAG_START;
                        if let Some(shown) = container.slot_mut(at) {
                            *shown = stack.clone();
                        }
                    }
                });
                client.write_packet(ServerboundSetCreativeModeSlot { slot_num: slot as u16, item_stack: stack });
                Ok(slot)
            })??;

            Ok(Answer::text(format!("Put {count} {item_name} in slot {slot}.")))
        })
    },
};

/// An item by the name a caller wrote, with or without the namespace. Another namespace is not an
/// item this version has.
fn item(name: &str) -> Option<ItemKind> {
    let id = name.trim().to_lowercase();
    let path = match id.split_once(':') {
        Some(("minecraft", path)) => path.to_owned(),
        Some(_) => return None,
        None => id,
    };
    ItemKind::from_str(&path).ok()
}

/// The hotbar first, the bag after: the other kind of bot fills the same slot for the same call.
fn first_empty(menu: &Menu) -> Result<usize, Failure> {
    (HOTBAR_START..=HOTBAR_END)
        .chain(BAG_START..HOTBAR_START)
        .find(|slot| menu.slot(*slot).is_some_and(ItemStack::is_empty))
        .ok_or_else(|| Failure::refused("INVENTORY_FULL", "The inventory is full and no slot was given."))
}
