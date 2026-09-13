mod args;
mod blocks;
mod chat;
mod player;
mod position;
mod wait;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use serde_json::{Value, json};

use crate::bot::Bot;
use crate::calls::{Failure, Outcome};
use crate::catalog;
use crate::game::Game;

pub use player::game_mode;

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
    wait::WAIT_TICKS,
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
