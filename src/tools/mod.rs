mod approach;
mod args;
mod blocks;
mod body;
mod book;
mod chat;
mod command;
mod dialogs;
mod editors;
mod entities;
mod hands;
mod hud;
mod input;
mod inventory;
mod options;
mod player;
mod position;
mod recipes;
mod stacks;
mod text;
mod trades;
mod wait;
mod walk;
mod windows;
mod world;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::bot::Bot;
use crate::calls::{Failure, Outcome};
use crate::catalog;
use crate::game::Game;

pub use player::game_mode;
pub use windows::received;

pub type Run = fn(Rc<Bot>, Value) -> Pin<Box<dyn Future<Output = Outcome>>>;

pub struct Tool {
    pub name: &'static str,
    pub run: Run,
}

const TOOLS: &[Tool] = &[
    position::GET_POSITION,
    player::GET_PLAYER_STATE,
    blocks::GET_BLOCK_INFO,
    blocks::FIND_BLOCKS,
    chat::SEND_CHAT,
    command::RUN_COMMAND,
    command::COMPLETE_COMMAND,
    command::SWITCH_SERVER,
    hud::READ_SCOREBOARD,
    hud::READ_BOSS_BARS,
    hud::READ_PLAYER_LIST,
    world::GET_WORLD_STATE,
    wait::WAIT_TICKS,
    chat::CLICK_CHAT,
    trades::READ_TRADES,
    trades::SELECT_TRADE,
    options::READ_CONTAINER_OPTIONS,
    options::PRESS_CONTAINER_BUTTON,
    options::SET_BEACON_EFFECTS,
    book::READ_BOOK,
    recipes::CAN_CRAFT,
    recipes::GET_RECIPE,
    recipes::LIST_RECIPES,
    recipes::CRAFT_ITEM,
    entities::FIND_ENTITY,
    entities::ATTACK_ENTITY,
    entities::INTERACT_ENTITY,
    inventory::LIST_INVENTORY,
    inventory::FIND_ITEM,
    inventory::EQUIP_ITEM,
    inventory::GIVE_ITEM,
    windows::OPEN_CONTAINER,
    windows::READ_WINDOW,
    windows::CLOSE_WINDOW,
    windows::WAIT_FOR_WINDOW,
    windows::CLICK_SLOT,
    windows::DRAG_SLOTS,
    windows::DROP_HELD_ITEM,
    body::LOOK_AT,
    body::JUMP,
    body::SET_STANCE,
    body::MOVE_IN_DIRECTION,
    body::RESPAWN,
    walk::MOVE_TO_POSITION,
    hands::DIG_BLOCK,
    hands::PLACE_BLOCK,
    hands::ACTIVATE_BLOCK,
    hands::USE_HELD_ITEM,
    input::PRESS_INPUT,
    dialogs::PRESS_DIALOG_BUTTON,
    dialogs::SET_DIALOG_INPUT,
    editors::TYPE_TEXT,
    editors::READ_BLOCK_ENTITY,
];

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

/// What this bot offers at the handshake: the tools it implements that the catalogue has a hash
/// for. A tool with no hash has nothing to agree with, so it is not offered.
pub fn capabilities() -> Vec<Value> {
    TOOLS
        .iter()
        .filter_map(|tool| catalog::args_hash(tool.name).map(|hash| json!({"tool": tool.name, "argsHash": hash})))
        .collect()
}

/// The world, for a tool that needs one: joined, and spawned into it.
pub fn in_world<T>(bot: &Bot, read: impl FnOnce(&Game) -> T) -> Result<T, Failure> {
    match bot.game.borrow().as_ref().filter(|game| game.spawned()) {
        Some(game) => Ok(read(game)),
        None => Err(Failure::not_in_game()),
    }
}

/// The world, for a tool that acts in it. A dead player is still a player to the client, and a tool
/// that went on acting for one sent the server nothing and waited out its deadline.
pub fn alive<T>(bot: &Bot, act: impl FnOnce(&Game) -> T) -> Result<T, Failure> {
    in_world(bot, |game| {
        if game.client.get_component::<azalea::entity::Dead>().is_some() {
            Err(Failure::dead(game.cause_of_death()))
        } else {
            Ok(act(game))
        }
    })?
}

/// The next tick the client runs, or a second of nothing.
///
/// A connection that ends stops the ticks, and a wait with no end of its own would sit until the
/// call's deadline and answer with a timeout instead of saying the bot left. The caller asks again
/// whether it is in a world after every one of these.
pub async fn tick(bot: &Bot) {
    let mut ticks = bot.ticks.subscribe();
    ticks.borrow_and_update();
    let _ = tokio::time::timeout(Duration::from_secs(1), ticks.changed()).await;
}
