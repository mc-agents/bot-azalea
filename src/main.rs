mod auth;
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
use crate::config::{Config, Microsoft};

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

    /*
    A subcommand and not another environment variable, because the two are started by different
    things: the operator's pod spec and `docker compose up` pass no arguments at all, so nothing
    either of them starts can land in the login by accident, and `docker compose run --rm bot-login`
    says on the command line which of the two is being run.
    */
    match std::env::args().nth(1).as_deref() {
        None => serve(&runtime),
        Some("login") => login(&runtime),
        Some(other) => fatal(format!("{other:?} is not a command; the only one is `login`")),
    }
}

/// The bot: sign in, dial the server, serve the link until the process is told to stop.
fn serve(runtime: &tokio::runtime::Runtime) {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(wrong) => fatal(wrong),
    };
    let account = match runtime.block_on(auth::sign_in(&config.auth)) {
        Ok(account) => account,
        Err(wrong) => fatal(wrong),
    };
    let bot = Rc::new(Bot::new(config, account));

    tokio::task::LocalSet::new().block_on(runtime, async move {
        tokio::task::spawn_local(health::serve(bot.clone()));
        link::run(bot).await;
    });
}

/// The Microsoft device flow, once, against whatever terminal is attached, and then out. It is its
/// own run of the image rather than something the bot does on demand because a login prints a link
/// and waits for somebody to open it, and inside a long-running bot both halves of that are lost:
/// the link goes to a log nobody is reading and the wait holds the call that asked for it.
///
/// `BOT_AUTH` is not consulted. This mode is the Microsoft flow by definition, and the service that
/// runs it is not the service that runs the bot.
fn login(runtime: &tokio::runtime::Runtime) {
    let microsoft = match Microsoft::from_env() {
        Ok(microsoft) => microsoft,
        Err(wrong) => fatal(wrong),
    };
    let username = match runtime.block_on(auth::login(&microsoft)) {
        Ok(username) => username,
        Err(wrong) => fatal(wrong),
    };
    tracing::info!(
        "logged in as {username}; the bot reads the credential from {}",
        microsoft.cache.display()
    );
}

/// A start-up that cannot go on. There is no link to report it over yet and nothing a probe could
/// answer that would be true, so the message is the whole of it and the process stops: a bot that
/// started anyway would answer every connect with a failure that looks like the game server's.
fn fatal(wrong: String) -> ! {
    tracing::error!("{wrong}");
    std::process::exit(1);
}
