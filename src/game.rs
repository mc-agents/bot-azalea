use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use azalea::Client;
use azalea::account::Account;
use azalea::app::{App, PluginGroup};
use azalea::auto_reconnect::AutoReconnectPlugin;
use azalea::auto_respawn::AutoRespawnPlugin;
use azalea::bot::DefaultBotPlugins;
use azalea::ecs::world::World;
use azalea::events::LocalPlayerEvents;
use azalea::join::{ConnectOpts, StartJoinServerEvent};
use azalea::protocol::address::ResolvableAddr;
use azalea::swarm::DefaultSwarmPlugins;
use azalea::task_pool::{TaskPoolOptions, TaskPoolPlugin};
use azalea::{DefaultPlugins, Event, start_ecs_runner};
use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use crate::bot::{Bot, now_millis};
use crate::calls::{self, Answer, Failure, Outcome};
use crate::feeds;

/// The world the bot is in, while it is in one.
pub struct Game {
    /// Which connection this is. A disconnect from the one before can arrive after this one has
    /// taken its place, and must not be read as this one ending.
    generation: u64,
    pub client: Client,
    pub address: String,
    pub username: String,
    spawned: Cell<bool>,
    /// Set when the server told us to leave, so the disconnect that follows reads as leaving and
    /// not as being dropped.
    leaving: Cell<bool>,
}

impl Game {
    pub fn spawned(&self) -> bool {
        self.spawned.get()
    }

    pub fn describe(&self, status: &mut Value) {
        status["address"] = json!(self.address);
        status["username"] = json!(self.username);
        if self.spawned() {
            let position = self.client.position();
            status["position"] = json!({"x": position.x, "y": position.y, "z": position.z});
        }
    }
}

/// The ECS every connection in this process runs in, made once.
///
/// azalea's own `Client::join` builds a new one per join, with its defaults: every core as a
/// Bevy worker, and a bot that respawns itself and rejoins by itself. The first cost a quarter of
/// a core per idle bot waking twenty-two threads sixty times a second -- one thread brought it to
/// three percent. The second is what the protocol forbids: a server under test may be checking
/// what happens on death or on a kick, and a bot that quietly got up again would hide exactly that.
fn ecs() -> Arc<RwLock<World>> {
    static ECS: OnceLock<Arc<RwLock<World>>> = OnceLock::new();
    ECS.get_or_init(|| {
        let mut app = App::new();
        app.add_plugins((
            DefaultPlugins.set(TaskPoolPlugin {
                task_pool_options: TaskPoolOptions { min_total_threads: 1, max_total_threads: 1, ..Default::default() },
            }),
            DefaultBotPlugins.build().disable::<AutoRespawnPlugin>().disable::<AutoReconnectPlugin>(),
            DefaultSwarmPlugins,
        ));
        let (ecs, start, _exit) = start_ecs_runner(app.main_mut());
        start();
        ecs
    })
    .clone()
}

pub fn connect(bot: &Rc<Bot>, message: &Value) {
    let id = message["id"].clone();
    let host = message["host"].as_str().unwrap_or("127.0.0.1").to_owned();
    let port = message["port"].as_u64().unwrap_or(25565) as u16;
    let username = message["username"].as_str().unwrap_or(&bot.config.bot_name).to_owned();
    let spawn_timeout = Duration::from_millis(message["spawnTimeoutMs"].as_u64().unwrap_or(60_000));

    let work = join(bot.clone(), host, port, username, spawn_timeout);
    calls::run(bot, id, "connect".into(), spawn_timeout + Duration::from_secs(2), work);
}

async fn join(bot: Rc<Bot>, host: String, port: u16, username: String, spawn_timeout: Duration) -> Outcome {
    leave(&bot);
    bot.status("connecting", None);

    let address = format!("{host}:{port}");
    let resolved = address
        .clone()
        .resolve()
        .await
        .map_err(|error| Failure::refused("JOIN_FAILED_DIAL", format!("{address} did not resolve: {error}")))?;

    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let client = start(Account::offline(&username), ConnectOpts {
        address: resolved,
        server_proxy: None,
        sessionserver_proxy: None,
    })
    .await;
    let (events, receiver) = mpsc::unbounded_channel();
    ecs().write().entity_mut(client.entity).insert(LocalPlayerEvents(events));

    *bot.game.borrow_mut() = Some(Game {
        generation,
        client,
        address: address.clone(),
        username: username.clone(),
        spawned: Cell::new(false),
        leaving: Cell::new(false),
    });

    let (joined, outcome) = oneshot::channel();
    tokio::task::spawn_local(pump(bot.clone(), generation, receiver, joined));

    match tokio::time::timeout(spawn_timeout, outcome).await {
        Ok(Ok(Ok(()))) => {
            bot.status("ready", None);
            Ok(Answer::text(format!("Joined {address} as {username}")))
        }
        Ok(Ok(Err(failure))) => Err(failure),
        _ => {
            leave(&bot);
            Err(Failure::refused(
                "JOIN_FAILED_SPAWN",
                format!("did not spawn on {address} within {}ms", spawn_timeout.as_millis()),
            ))
        }
    }
}

/// Join one connection to the shared ECS. What `Client::start_client` does, which the released crate
/// keeps behind a private options type; every piece of it is public on its own.
async fn start(account: Account, connect_opts: ConnectOpts) -> Client {
    let ecs = ecs();
    let (callback, mut entity) = mpsc::unbounded_channel();

    ecs.write().write_message(StartJoinServerEvent {
        account,
        connect_opts,
        start_join_callback_tx: Some(callback),
    });

    let entity = entity.recv().await.expect("azalea drops the join callback only when its ECS has stopped");
    Client::new(entity, ecs)
}

pub fn disconnect(bot: &Rc<Bot>, message: &Value) {
    let id = message["id"].clone();
    let bot_for_work = bot.clone();
    calls::run(bot, id, "disconnect".into(), Duration::from_secs(10), async move {
        leave(&bot_for_work);
        /* Told to leave, so nothing went wrong: linked and in no world is idle. */
        bot_for_work.status("idle", Some("asked to leave"));
        Ok(Answer::text("Left the server"))
    });
}

fn leave(bot: &Bot) {
    if let Some(game) = bot.game.borrow_mut().take() {
        game.leaving.set(true);
        game.client.disconnect();
    }
}

/// Everything azalea says about one connection, until it ends.
async fn pump(
    bot: Rc<Bot>,
    generation: u64,
    mut events: mpsc::UnboundedReceiver<Event>,
    joined: oneshot::Sender<Result<(), Failure>>,
) {
    let mut joined = Some(joined);
    let mut logged_in = false;
    let mine = |bot: &Bot| bot.game.borrow().as_ref().is_some_and(|game| game.generation == generation);

    while let Some(event) = events.recv().await {
        match event {
            Event::Login => logged_in = true,
            Event::Spawn => {
                if let Some(game) = bot.game.borrow().as_ref().filter(|game| game.generation == generation) {
                    game.spawned.set(true);
                }
                if let Some(joined) = joined.take() {
                    let _ = joined.send(Ok(()));
                }
            }
            Event::Tick => bot.ticks.send_modify(|ticks| *ticks += 1),
            Event::Chat(packet) => feeds::chat(&bot, &packet),
            Event::ConnectionFailed(error) => {
                if let Some(joined) = joined.take() {
                    let _ = joined.send(Err(Failure::refused("JOIN_FAILED_DIAL", error.to_string())));
                }
                break;
            }
            Event::Disconnect(reason) => {
                let reason = reason.map(|text| text.to_string()).unwrap_or_else(|| "disconnected".into());
                let dropped = mine(&bot) && !bot.game.borrow().as_ref().is_some_and(|game| game.leaving.get());

                if let Some(joined) = joined.take() {
                    /* Kicked before spawning is a login refused; after logging in, a world that never came. */
                    let code = if logged_in { "JOIN_FAILED_SPAWN" } else { "JOIN_FAILED_LOGIN" };
                    let _ = joined.send(Err(Failure::refused(code, reason.clone())));
                } else if dropped {
                    bot.status("disconnected", Some(&reason));
                }
                if mine(&bot) {
                    bot.game.borrow_mut().take();
                }
                info!("disconnected at {}: {reason}", now_millis());
                break;
            }
            _ => {}
        }
    }
}
