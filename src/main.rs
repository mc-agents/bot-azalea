mod bot;
mod calls;
mod catalog;
mod config;
mod dialog;
mod editors;
mod feeds;
mod game;
mod health;
mod hud;
mod link;
mod menus;
mod text;
mod tools;
mod worldedit;

use std::rc::Rc;

use crate::bot::Bot;
use crate::config::Config;

/// One thread for the link, every call and the game. azalea runs its ECS in a local task set, and a
/// bot that is one process among fifty should not fan out a worker per core to do it.
fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    tracing::info!(
        "bot-azalea {} with catalogue {} from mcp-server {}",
        env!("CARGO_PKG_VERSION"),
        catalog::CATALOG_VERSION,
        catalog::CATALOG_SOURCE
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a single-threaded runtime");

    let bot = Rc::new(Bot::new(Config::from_env()));

    tokio::task::LocalSet::new().block_on(&runtime, async move {
        tokio::task::spawn_local(health::serve(bot.clone()));
        link::run(bot).await;
    });
}
