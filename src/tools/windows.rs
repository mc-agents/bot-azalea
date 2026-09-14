use std::time::{Duration, Instant};

use azalea::container::ContainerHandleRef;
use azalea::entity::inventory::Inventory;
use azalea::interact::SwingArmEvent;
use azalea::inventory::operations::ClickType;
use azalea::inventory::{ItemStack, Menu};
use azalea::protocol::packets::game::ClientboundGamePacket;
use azalea::protocol::packets::game::s_client_command::{Action, ServerboundClientCommand};
use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
use azalea::registry::builtin::{BlockKind, MenuKind};
use azalea::{BlockPos, Client, FormattedText, Vec3};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use super::approach::Approach;
use super::args::{boolean, integer, plain, position, text, text_or, written};
use super::text::component;
use super::{Tool, alive, in_world, stacks, tick};
use crate::bot::Bot;
use crate::menus::Menus;
use crate::calls::{Answer, Failure, Outcome};

/// A player inventory is 36 slots wherever it is attached.
const PLAYER_INVENTORY_SLOTS: usize = 36;

/// Clicking outside the window, where a drop and a drag's two ends go. The number is the protocol's.
const OUTSIDE: i16 = -999;

/// A state id the server never holds: it counts to 32767 and wraps.
///
/// A click sent with it is still a click -- the server applies it -- and the mismatch makes the
/// server answer with the whole window, cursor included, instead of only what it thinks changed.
/// That answer is what a click waits for. azalea predicts a click before sending it, and for a
/// number-key swap and for a drag its prediction is not the game's: a swap takes the key for a
/// window slot, and a drag onto an empty slot moves nothing. Read straight after, the slot and the
/// cursor said what azalea guessed; read after the answer, they say what the server did.
const RESYNC: u32 = 32_768;

/// How long a click waits for that answer. A server that drops the click -- a window it already
/// closed -- sends none, and the tool reads what there is rather than timing out.
const SETTLE: Duration = Duration::from_secs(2);

/// Ticks between clicking a container and giving up on its window.
const OPEN_TICKS: u32 = 100;

const CONTAINER_BLOCKS: &[&str] =
    &["chest", "trapped_chest", "ender_chest", "barrel", "hopper", "dispenser", "dropper", "shulker_box"];

/// What azalea leaves out of the packets that fill a window, put back.
///
/// It takes the slots from a full window and drops the state id and the cursor that come with
/// them, and it ignores the cursor packet 26.x sends on its own. A click then went out with a stale
/// state id, and the cursor a tool read was whatever azalea last guessed it to be.
pub fn received(bot: &Bot, client: &Client, packet: &ClientboundGamePacket) {
    match packet {
        ClientboundGamePacket::OpenScreen(_) => {
            bot.contents.send_replace(None);
        }
        ClientboundGamePacket::ContainerSetContent(content) => {
            let _ = client.try_query_self::<&mut Inventory, _>(|mut inventory| {
                if inventory.id == content.container_id {
                    inventory.state_id = content.state_id;
                    inventory.carried = content.carried_item.clone();
                }
            });
            bot.contents.send_replace(Some(content.container_id));
        }
        ClientboundGamePacket::SetCursorItem(cursor) => {
            let _ = client.try_query_self::<&mut Inventory, _>(|mut inventory| {
                inventory.carried = cursor.contents.clone();
            });
        }
        _ => {}
    }
}

/// A menu the server opened. The player's own inventory is not one, because nothing here opens it.
pub(super) struct Window {
    pub(super) id: i32,
    pub(super) menu: Menu,
    pub(super) title: FormattedText,
}

/// The menu the server opened, whatever draws it.
pub(super) fn menu(client: &Client) -> Option<Window> {
    let inventory = client.get_component::<Inventory>()?;
    Some(Window {
        id: inventory.id,
        menu: inventory.container_menu.clone()?,
        title: inventory.container_menu_title.clone().unwrap_or_default(),
    })
}

/// An open window: a menu drawn as slots. A lectern is a menu the game draws as a book, and every
/// tool that reads or clicks a window finds nothing open in front of one, as a player's client does.
fn open(client: &Client) -> Option<Window> {
    menu(client).filter(|window| !matches!(window.menu, Menu::Lectern { .. }))
}

/// The client's own name for the screen a menu is drawn by, which is what a refusal names when the
/// screen is the wrong one.
pub(super) fn screen(menu: &Menu) -> &'static str {
    match menu {
        Menu::Generic9x1 { .. }
        | Menu::Generic9x2 { .. }
        | Menu::Generic9x3 { .. }
        | Menu::Generic9x4 { .. }
        | Menu::Generic9x5 { .. }
        | Menu::Generic9x6 { .. } => "ContainerScreen",
        Menu::Generic3x3 { .. } => "DispenserScreen",
        Menu::Crafter3x3 { .. } => "CrafterScreen",
        Menu::Anvil { .. } => "AnvilScreen",
        Menu::Beacon { .. } => "BeaconScreen",
        Menu::BlastFurnace { .. } => "BlastFurnaceScreen",
        Menu::BrewingStand { .. } => "BrewingStandScreen",
        Menu::Crafting { .. } => "CraftingScreen",
        Menu::Enchantment { .. } => "EnchantmentScreen",
        Menu::Furnace { .. } => "FurnaceScreen",
        Menu::Grindstone { .. } => "GrindstoneScreen",
        Menu::Hopper { .. } => "HopperScreen",
        Menu::Lectern { .. } => "LecternScreen",
        Menu::Loom { .. } => "LoomScreen",
        Menu::Merchant { .. } => "MerchantScreen",
        Menu::ShulkerBox { .. } => "ShulkerBoxScreen",
        Menu::Smithing { .. } => "SmithingScreen",
        Menu::Smoker { .. } => "SmokerScreen",
        Menu::CartographyTable { .. } => "CartographyTableScreen",
        Menu::Stonecutter { .. } => "StonecutterScreen",
        Menu::Player(_) => "InventoryScreen",
    }
}

/// A window whose contents have arrived. The server opens a window and fills it in two packets,
/// and one read between them describes an empty chest.
fn filled(bot: &Bot, client: &Client) -> Option<Window> {
    open(client).filter(|window| *bot.contents.borrow() == Some(window.id))
}

fn require(client: &Client) -> Result<Window, Failure> {
    open(client).ok_or_else(|| {
        Failure::refused(
            "NO_WINDOW",
            "No window is open. Run the command that opens the menu first, then use wait-for-window before reading or clicking it.",
        )
    })
}

fn describe(window: &Window) -> Value {
    let slots = window.menu.slots();
    let filled: Vec<Value> = slots
        .iter()
        .enumerate()
        .filter(|(_, stack)| !stack.is_empty())
        .map(|(index, stack)| stacks::slot(stack, index))
        .collect();

    let slot_count = slots.len();
    let inventory_start = slot_count.saturating_sub(PLAYER_INVENTORY_SLOTS);

    json!({
        "title": window.title.to_string(),
        /* A menu header is drawn in the pack's own font as often as an item name is. */
        "titleComponent": component(&window.title),
        "type": kind(&window.menu),
        "slotCount": slot_count,
        "containerSlots": [0, inventory_start.saturating_sub(1)],
        "inventorySlots": [inventory_start, slot_count.saturating_sub(1)],
        "filled": filled,
    })
}

/// The registry name of a window's type. azalea keeps the menu and not the type it was opened as,
/// and the two name each other one to one.
pub(super) fn kind(menu: &Menu) -> &'static str {
    let kind = match menu {
        Menu::Player(_) => return "minecraft:inventory",
        Menu::Generic9x1 { .. } => MenuKind::Generic9x1,
        Menu::Generic9x2 { .. } => MenuKind::Generic9x2,
        Menu::Generic9x3 { .. } => MenuKind::Generic9x3,
        Menu::Generic9x4 { .. } => MenuKind::Generic9x4,
        Menu::Generic9x5 { .. } => MenuKind::Generic9x5,
        Menu::Generic9x6 { .. } => MenuKind::Generic9x6,
        Menu::Generic3x3 { .. } => MenuKind::Generic3x3,
        Menu::Crafter3x3 { .. } => MenuKind::Crafter3x3,
        Menu::Anvil { .. } => MenuKind::Anvil,
        Menu::Beacon { .. } => MenuKind::Beacon,
        Menu::BlastFurnace { .. } => MenuKind::BlastFurnace,
        Menu::BrewingStand { .. } => MenuKind::BrewingStand,
        Menu::Crafting { .. } => MenuKind::Crafting,
        Menu::Enchantment { .. } => MenuKind::Enchantment,
        Menu::Furnace { .. } => MenuKind::Furnace,
        Menu::Grindstone { .. } => MenuKind::Grindstone,
        Menu::Hopper { .. } => MenuKind::Hopper,
        Menu::Lectern { .. } => MenuKind::Lectern,
        Menu::Loom { .. } => MenuKind::Loom,
        Menu::Merchant { .. } => MenuKind::Merchant,
        Menu::ShulkerBox { .. } => MenuKind::ShulkerBox,
        Menu::Smithing { .. } => MenuKind::Smithing,
        Menu::Smoker { .. } => MenuKind::Smoker,
        Menu::CartographyTable { .. } => MenuKind::CartographyTable,
        Menu::Stonecutter { .. } => MenuKind::Stonecutter,
    };
    kind.to_str()
}

fn out_of_range(slot: i64, menu: &Menu) -> Failure {
    Failure::refused(
        "SLOT_OUT_OF_RANGE",
        format!("Slot {slot} is outside the window, whose slots run 0-{}", menu.len() as i64 - 1),
    )
}

/// Click a slot of a window, and come back once the server has said what the click did.
///
/// `window` is the one the click is meant for. The server ignores a click for any other, and says
/// nothing, so there is no answer to wait for then.
pub async fn click(bot: &Bot, window: i32, slot: i16, button: u8, click_type: ClickType) -> Result<(), Failure> {
    let mut contents = bot.contents.subscribe();
    contents.borrow_and_update();

    let answered = alive(bot, |game| {
        let client = &game.client;
        let (open, carried) = {
            let inventory = client.component::<Inventory>();
            (inventory.id, inventory.carried.clone())
        };
        let carried = HashedStack::from_item_stack(&carried, &client.world().read().registries);

        client.write_packet(ServerboundContainerClick {
            container_id: window,
            state_id: RESYNC,
            slot_num: slot,
            button_num: button,
            click_type,
            changed_slots: IndexMap::new(),
            carried_item: carried,
        });
        open == window
    })?;

    if answered {
        let _ = tokio::time::timeout(SETTLE, async {
            while contents.changed().await.is_ok() {
                if *contents.borrow_and_update() == Some(window) {
                    break;
                }
            }
        })
        .await;
    }
    Ok(())
}

/// Ask the server for the whole window again, with a click that changes nothing: a middle-click
/// outside it, which only a creative player with an empty cursor on a slot makes anything of. A menu
/// action that is not a click -- picking a trade, pressing a menu button -- is answered with only what
/// changed, or with nothing, and this is what a tool waits on before it reads what the action did.
pub(super) async fn resync(bot: &Bot, window: i32) -> Result<(), Failure> {
    click(bot, window, OUTSIDE, 0, ClickType::Clone).await
}

/// A stack in the player's own inventory by its index there: 0-8 the hotbar, 40 the off-hand. A
/// number-key swap names its second stack this way, and the off-hand is in no container window.
fn inventory_item(inventory: &Inventory, index: u8) -> ItemStack {
    match index {
        0..=8 => {
            let menu = inventory.menu();
            menu.slot(*menu.hotbar_slots_range().start() + index as usize).cloned().unwrap_or_default()
        }
        _ => inventory.inventory_menu.as_player().offhand.clone(),
    }
}

pub const READ_WINDOW: Tool = Tool {
    name: "read-window",
    run: |bot, _args| {
        Box::pin(async move {
            /* Nothing being open is a state, so it travels as window: null rather than as a refusal. */
            let window = in_world(&bot, |game| open(&game.client).map(|window| describe(&window)))?;
            Ok(Answer::data("read-window", json!({"window": window})))
        })
    },
};

pub const CLOSE_WINDOW: Tool = Tool {
    name: "close-window",
    run: |bot, _args| {
        Box::pin(async move {
            /*
            The credits first: they are drawn over whatever was open. Escape on them is a respawn,
            which is what takes the player out of the End, and they have no title of their own.
            */
            let credits = in_world(&bot, |game| {
                let showing = std::mem::take(&mut game.hud.borrow_mut().credits);
                if showing {
                    game.client.write_packet(ServerboundClientCommand { action: Action::PerformRespawn });
                }
                showing
            })?;
            if credits {
                return Ok(Answer::data("close-window", untitled("end credits")));
            }

            /* A book is drawn over the world rather than as a menu, and closing it tells the server nothing. */
            let book = in_world(&bot, |game| {
                game.client
                    .try_query_self::<&mut Menus, _>(|mut menus| menus.book.take().is_some())
                    .unwrap_or_default()
            })?;
            if book {
                return Ok(Answer::data("close-window", untitled("book")));
            }

            /* A dialog is drawn over any window, and Escape closes it first, running its exit button. */
            let dialog = in_world(&bot, |game| {
                let mut hud = game.hud.borrow_mut();
                let Some(dialog) = hud.dialog.as_ref() else { return Ok(None) };
                if !dialog.closes_on_escape() {
                    return Err(Failure::refused(
                        "SCREEN_STAYS_OPEN",
                        "the dialog does not close on Escape, so it is not closed from here either.",
                    ));
                }
                let title = dialog.title();
                if let Some(exit) = dialog.exit()
                    && let Some(action) = &exit.action
                {
                    /* What the exit button runs is not what was asked for, so a command it will not run is not refused here. */
                    let _ = super::dialogs::run(&game.client, hud.commands.as_ref(), dialog, action, &exit.label);
                }
                hud.dialog = None;
                Ok(Some(title))
            })??;
            if let Some((title, title_component)) = dialog {
                return Ok(Answer::data(
                    "close-window",
                    json!({"closed": title, "closedComponent": title_component, "screen": "dialog"}),
                ));
            }
            if let Some((title, screen)) = in_world(&bot, super::editors::close)? {
                return Ok(Answer::data("close-window", json!({"closed": title, "closedComponent": title, "screen": screen})));
            }

            /* Asking to close nothing is a no-op, not a mistake. */
            let Some(window) = in_world(&bot, |game| menu(&game.client))? else {
                return Ok(Answer::data("close-window", json!({"closed": null, "screen": null})));
            };

            alive(&bot, |game| ContainerHandleRef::new(window.id, game.client.clone()).close())?;
            if matches!(window.menu, Menu::Lectern { .. }) {
                return Ok(Answer::data("close-window", untitled("lectern")));
            }
            Ok(Answer::data(
                "close-window",
                json!({
                    "closed": window.title.to_string(),
                    "closedComponent": component(&window.title),
                    "screen": null,
                }),
            ))
        })
    },
};

/// A screen with no title of its own, closed: the book screen has none, and neither does a lectern's.
fn untitled(screen: &str) -> Value {
    json!({"closed": "", "closedComponent": component(&FormattedText::default()), "screen": screen})
}

pub const WAIT_FOR_WINDOW: Tool = Tool {
    name: "wait-for-window",
    run: |bot, args| {
        Box::pin(async move {
            let source = match &args["titlePattern"] {
                Value::Null => None,
                _ => Some(text(&args, "titlePattern")?.to_owned()),
            };
            let timeout_ms = integer(&args, "timeoutMs", 10_000)?;

            /*
            The catalogue calls this a JavaScript regular expression, because the first bot to take
            it was written in TypeScript. The subset a caller writes for a window title is the same
            here, and `is_match` finds anywhere in the title the way RegExp.test does.
            */
            let pattern = source
                .as_deref()
                .map(Regex::new)
                .transpose()
                .map_err(|invalid| {
                    Failure::bad_args(format!(
                        "\"{}\" is not a valid regular expression: {invalid}",
                        source.as_deref().unwrap_or_default()
                    ))
                })?;

            let started = Instant::now();
            let window = loop {
                let found = in_world(&bot, |game| {
                    filled(&bot, &game.client)
                        .filter(|window| pattern.as_ref().is_none_or(|pattern| pattern.is_match(&window.title.to_string())))
                        .map(|window| describe(&window))
                })?;
                if found.is_some() || started.elapsed().as_millis() >= timeout_ms as u128 {
                    break found;
                }
                tick(&bot).await;
            };

            /* Nothing opening in time is a state and not a timeout: the call's own deadline is that. */
            let summary = if window.is_some() { "window opened".to_owned() } else { format!("nothing opened within {timeout_ms}ms") };
            Ok(Answer::data(summary, json!({"titlePattern": source, "timeoutMs": timeout_ms, "window": window})))
        })
    },
};

pub const OPEN_CONTAINER: Tool = Tool {
    name: "open-container",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;
            let corner = Vec3::new(at.x as f64, at.y as f64, at.z as f64);

            let mut approach = Approach::new();
            while !approach.reached(&bot, corner, &written(at))? {
                tick(&bot).await;
            }
            drop(approach);

            alive(&bot, |game| use_container(&game.client, at))??;
            /* The swing after the click, as a player's arm follows the click: azalea sends the click on the next tick. */
            tick(&bot).await;
            alive(&bot, |game| swing(&game.client))?;

            /*
            Nothing opening is a refusal and not a state: the caller named a container and asked what
            is in it, and an answer describing no window answers another question.
            */
            for _ in 0..OPEN_TICKS {
                if let Some(window) = in_world(&bot, |game| filled(&bot, &game.client))? {
                    return Ok(Answer::data(format!("window \"{}\"", window.title), describe(&window)));
                }
                tick(&bot).await;
            }
            Err(Failure::refused(
                "NO_WINDOW_OPENED",
                format!("the container at {} was clicked but no window opened within 5s", written(at)),
            ))
        })
    },
};

/// Right-click the block, if it is one of the containers the other kinds of bot also accept. A block
/// a plugin opens a menu from is activate-block and wait-for-window.
fn use_container(client: &Client, at: BlockPos) -> Result<(), Failure> {
    let block = client.world().read().get_block_state(at).map(BlockKind::from).unwrap_or(BlockKind::Air);
    let name = plain(block.to_str());

    if !CONTAINER_BLOCKS.contains(&name) && !name.ends_with("_shulker_box") {
        return Err(Failure::refused("NOT_A_CONTAINER", format!("{} holds {name}, not a container", written(at))));
    }

    client.look_at(at.center());
    client.block_interact(at);
    Ok(())
}

pub fn swing(client: &Client) {
    client.ecs.write().trigger(SwingArmEvent { entity: client.entity });
}

pub const CLICK_SLOT: Tool = Tool {
    name: "click-slot",
    run: |bot, args| {
        Box::pin(async move {
            let slot = integer(&args, "slot", -1)?;
            let outside = boolean(&args, "outside", false)?;
            let button = text(&args, "button")?.to_owned();
            let shift = boolean(&args, "shift", false)?;
            let mode = text_or(&args, "mode", "click")?.to_owned();
            let hotbar = integer(&args, "hotbar", 0)?;

            let window = alive(&bot, |game| require(&game.client))??;

            if outside {
                if slot >= 0 || mode != "click" || shift {
                    return Err(Failure::bad_args("a click outside the window is a plain click, and takes no slot, mode or shift"));
                }
                return click_outside(&bot, &window, button).await;
            }
            if slot < 0 {
                return Err(Failure::bad_args("click-slot needs a slot, or outside for a click outside the window"));
            }

            if slot < 0 || slot >= window.menu.len() as i64 {
                return Err(out_of_range(slot, &window.menu));
            }
            let (click_type, key) = input(&mode, &button, shift, hotbar)?;
            let swap = (click_type == ClickType::Swap).then_some(key);

            let before = window.menu.slot(slot as usize).cloned().unwrap_or_default();
            let swapped_before =
                swap.map(|index| in_world(&bot, |game| inventory_item(&game.client.component::<Inventory>(), index))).transpose()?;

            click(&bot, window.id, slot as i16, key, click_type).await?;

            let (after, cursor, swapped_after) = alive(&bot, |game| {
                let client = &game.client;
                let (after, cursor) = {
                    let inventory = client.component::<Inventory>();
                    (inventory.menu().slot(slot as usize).cloned().unwrap_or_default(), inventory.carried.clone())
                };

                /*
                The off-hand is in no container window, so the server's answer does not carry it, and
                what went there is read off the slot instead: a swap that happened left the slot
                holding what the off-hand had, and the off-hand holding what the slot had.
                */
                if swap == Some(OFF_HAND) && swapped_before.as_ref().is_some_and(|had| *had == after && after != before) {
                    let _ = client.try_query_self::<&mut Inventory, _>(|mut inventory| {
                        inventory.inventory_menu.as_player_mut().offhand = before.clone();
                    });
                }
                let swapped_after = swap.map(|index| inventory_item(&client.component::<Inventory>(), index));
                (after, cursor, swapped_after)
            })?;

            let swapped = match (swapped_before, swapped_after) {
                (Some(before), Some(after)) => json!({"before": stacks::held(&before), "after": stacks::held(&after)}),
                _ => Value::Null,
            };

            Ok(Answer::data(
                "click-slot",
                json!({
                    "slot": slot,
                    "outside": false,
                    "button": button,
                    "shift": shift,
                    "mode": mode,
                    "hotbar": if hotbar == 0 { Value::Null } else { json!(hotbar) },
                    "before": stacks::held(&before),
                    "after": stacks::held(&after),
                    "cursor": stacks::held(&cursor),
                    "swapped": swapped,
                }),
            ))
        })
    },
};

/// A click outside the window drops the cursor: all of it on the left button, one item on the right.
/// It lands on no slot, so what goes back as before and after is the cursor.
async fn click_outside(bot: &Bot, window: &Window, button: String) -> Outcome {
    let before = in_world(bot, |game| game.client.component::<Inventory>().carried.clone())?;

    click(bot, window.id, OUTSIDE, u8::from(button == "right"), ClickType::Pickup).await?;

    let after = in_world(bot, |game| game.client.component::<Inventory>().carried.clone())?;
    Ok(Answer::data(
        "click-slot",
        json!({
            "slot": null,
            "outside": true,
            "button": button,
            "shift": false,
            "mode": "click",
            "hotbar": null,
            "before": stacks::held(&before),
            "after": stacks::held(&after),
            "cursor": stacks::held(&after),
            "swapped": null,
        }),
    ))
}

/// The off-hand's index in the player's inventory, which is what a swap's button names.
pub const OFF_HAND: u8 = 40;

/// What a mode sends: the click type, and a button whose meaning is the type's. A swap's button is
/// an inventory index -- the "1" key is 0 -- a throw's is 0 for one item and 1 for the stack.
///
/// A shift or a right button given to anything but a click is refused rather than dropped. Every
/// one of them would still send something, and it would be a different input from the one asked for.
fn input(mode: &str, button: &str, shift: bool, hotbar: i64) -> Result<(ClickType, u8), Failure> {
    if mode != "click" && (shift || button != "left") {
        return Err(Failure::bad_args(format!("button and shift shape a click, and {mode} takes neither")));
    }
    if (mode == "swap-hotbar") != (hotbar != 0) {
        return Err(Failure::bad_args("hotbar names the key for swap-hotbar, and only swap-hotbar takes one"));
    }

    Ok(match mode {
        "click" => (if shift { ClickType::QuickMove } else { ClickType::Pickup }, u8::from(button == "right")),
        "swap-hotbar" => (ClickType::Swap, (hotbar - 1) as u8),
        "swap-offhand" => (ClickType::Swap, OFF_HAND),
        "throw-one" => (ClickType::Throw, 0),
        "throw-stack" => (ClickType::Throw, 1),
        "pickup-all" => (ClickType::PickupAll, 0),
        "clone" => (ClickType::Clone, 0),
        other => return Err(Failure::bad_args(format!("unknown mode {other}"))),
    })
}

pub const DRAG_SLOTS: Tool = Tool {
    name: "drag-slots",
    run: |bot, args| {
        Box::pin(async move {
            let button = text(&args, "button")?.to_owned();
            let kind: u8 = match button.as_str() {
                "left" => 0,
                "right" => 1,
                "middle" => 2,
                other => return Err(Failure::bad_args(format!("unknown button {other}"))),
            };

            let window = alive(&bot, |game| require(&game.client))??;
            let slots = drag_slots(&args, &window.menu)?;

            let before: Vec<ItemStack> =
                slots.iter().map(|slot| window.menu.slot(*slot).cloned().unwrap_or_default()).collect();
            let carried = in_world(&bot, |game| game.client.component::<Inventory>().carried.clone())?;

            /*
            A drag is three phases on both sides of the connection -- a start outside the window, a
            packet per slot, an end outside again -- and the button carries the phase in its low two
            bits and the kind of drag in the two above. A wrong packing is refused nowhere: the menu
            resets its drag and nothing moves. All of it goes in one call, because any other click
            between the start and the end resets the drag on the server.
            */
            let mask = |phase: u8| phase | (kind << 2);
            click(&bot, window.id, OUTSIDE, mask(0), ClickType::QuickCraft).await?;
            for slot in &slots {
                click(&bot, window.id, *slot as i16, mask(1), ClickType::QuickCraft).await?;
            }
            click(&bot, window.id, OUTSIDE, mask(2), ClickType::QuickCraft).await?;

            let (after, cursor) = in_world(&bot, |game| {
                let inventory = game.client.component::<Inventory>();
                let after: Vec<ItemStack> =
                    slots.iter().map(|slot| inventory.menu().slot(*slot).cloned().unwrap_or_default()).collect();
                (after, inventory.carried.clone())
            })?;

            let results: Vec<Value> = slots
                .iter()
                .zip(before.iter().zip(&after))
                .map(|(slot, (before, after))| json!({"slot": slot, "before": stacks::held(before), "after": stacks::held(after)}))
                .collect();

            Ok(Answer::data(
                "drag-slots",
                json!({
                    "button": button,
                    "slots": results,
                    "carried": stacks::held(&carried),
                    "cursor": stacks::held(&cursor),
                }),
            ))
        })
    },
};

/// A slot named twice is refused because the menu keeps a set: the second mention would be ignored
/// there and still show up here as a slot the drag reached twice.
fn drag_slots(args: &Value, menu: &Menu) -> Result<Vec<usize>, Failure> {
    let given = args["slots"].as_array().filter(|slots| !slots.is_empty());
    let given = given.ok_or_else(|| Failure::bad_args("expected a non-empty array of slots"))?;

    let mut slots = Vec::with_capacity(given.len());
    for element in given {
        let slot = element
            .as_i64()
            .or_else(|| element.as_f64().filter(|number| number.fract() == 0.0).map(|number| number as i64))
            .ok_or_else(|| Failure::bad_args(format!("expected an integer in slots, got {element}")))?;
        if slot < 0 || slot >= menu.len() as i64 {
            return Err(out_of_range(slot, menu));
        }
        if slots.contains(&(slot as usize)) {
            return Err(Failure::bad_args(format!("slot {slot} is listed twice")));
        }
        slots.push(slot as usize);
    }
    Ok(slots)
}

pub const DROP_HELD_ITEM: Tool = Tool {
    name: "drop-held-item",
    run: |bot, args| {
        Box::pin(async move {
            /*
            With no window open the menu is the player's own inventory, which is where the cursor
            lives then. The cursor is emptied by clicking outside the window with the whole stack; a
            named slot is ctrl-Q, which needs no trip through the cursor.
            */
            let (window, menu, carried) = alive(&bot, |game| {
                let inventory = game.client.component::<Inventory>();
                (inventory.id, inventory.menu().clone(), inventory.carried.clone())
            })?;

            let (slot, dropped, click_at) = match &args["slot"] {
                Value::Null => (Value::Null, carried, (OUTSIDE, 0, ClickType::Pickup)),
                _ => {
                    let index = integer(&args, "slot", -1)?;
                    if index < 0 || index >= menu.len() as i64 {
                        return Err(out_of_range(index, &menu));
                    }
                    let stack = menu.slot(index as usize).cloned().unwrap_or_default();
                    (json!(index), stack, (index as i16, 1, ClickType::Throw))
                }
            };

            /* Nothing to drop is a state: it travels as dropped: null and mcp-server says so. */
            if !dropped.is_empty() {
                let (at, button, click_type) = click_at;
                click(&bot, window, at, button, click_type).await?;
            }
            Ok(Answer::data("drop-held-item", json!({"slot": slot, "dropped": stacks::held(&dropped)})))
        })
    },
};
