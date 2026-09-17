use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use azalea::Client;
use azalea::account::Account;
use azalea::app::{App, PluginGroup};
use azalea::auto_reconnect::AutoReconnectPlugin;
use azalea::auto_respawn::AutoRespawnPlugin;
use azalea::bot::DefaultBotPlugins;
use azalea::connection::RawConnection;
use azalea::ecs::world::World;
use azalea::events::LocalPlayerEvents;
use azalea::join::{ConnectOpts, StartJoinServerEvent};
use azalea::player::GameProfileComponent;
use azalea::protocol::address::ResolvableAddr;
use azalea::protocol::packets::game::ClientboundGamePacket;
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
use crate::hud::{self, Hud, HudPackets, HudPlugin};
use crate::menus::{Known, Menus, MenusPlugin};

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
    cause_of_death: RefCell<Option<String>>,
    pub hud: RefCell<Hud>,
    /// Closes when this connection's pump returns, which is once azalea has said it disconnected.
    ended: oneshot::Receiver<()>,
}

/// A connection that was told to end and may not have yet.
///
/// azalea keeps one entity per account and refuses to start a join on it while its connection is
/// alive, answering Ok and doing nothing. A rejoin under the same name sent straight after a leave
/// reached it in that window, so the bot sat waiting for a spawn that was never asked for.
pub struct Departing {
    client: Client,
    ended: oneshot::Receiver<()>,
}

/// How long a connection that was told to end is given to go. The disconnect is an ECS frame away.
const TEARDOWN: Duration = Duration::from_secs(5);

impl Game {
    /// Spawned, and still holding the world. A connection that ends strips the player of its world
    /// before the disconnect reaches the pump, and anything that read it in between -- the status
    /// the disconnect itself sends -- panicked on a position that was no longer there.
    pub fn spawned(&self) -> bool {
        self.spawned.get() && self.client.get_component::<azalea::InGameState>().is_some()
    }

    /// What killed the bot, while it is dead: the message the server sent with the death.
    pub fn cause_of_death(&self) -> Option<String> {
        self.cause_of_death.borrow().clone()
    }

    /// A proxy moving the bot takes the connection back to configuration, and azalea takes the
    /// player's components away until the next login -- a tool that read a position then would
    /// find none and take the process down with it.
    pub fn reconfiguring(&self) {
        self.spawned.set(false);
    }

    pub fn arrived(&self) {
        self.spawned.set(true);
    }

    /// Up again, so the last death no longer describes the bot.
    pub fn forget_death(&self) {
        self.cause_of_death.borrow_mut().take();
    }

    pub fn describe(&self, status: &mut Value) {
        status["address"] = json!(self.address);
        status["username"] = json!(self.username);
        if self.spawned() {
            let position = self.client.position();
            status["position"] = json!({"x": position.x, "y": position.y, "z": position.z});
            status["dead"] = json!(self.client.get_component::<azalea::entity::Dead>().is_some());
            if let Some(cause) = self.cause_of_death() {
                status["causeOfDeath"] = json!(cause);
            }
            status["gameMode"] = json!(crate::tools::game_mode(
                self.client
                    .get_component::<azalea::local_player::LocalGameMode>()
                    .map(|mode| mode.current)
            ));
            if let Some((_, dimension)) = self.hud.borrow().dimension() {
                status["dimension"] = json!(dimension.to_string());
            }
        }
    }
}

/// Send the status again when what a caller reads from it has changed, and every few seconds anyway.
///
/// A status went out on a join, a death and a disconnect only. A move between servers or
/// dimensions behind a proxy changes none of those, so get-bot-status went on naming the server and
/// the position the bot had left.
fn notice_world(bot: &Rc<Bot>, generation: u64) {
    /* Five seconds of client ticks. */
    const REFRESH_TICKS: u64 = 100;

    let seen = {
        let game = bot.game.borrow();
        let Some(game) = game
            .as_ref()
            .filter(|game| game.generation == generation && game.spawned())
        else {
            *bot.reported.borrow_mut() = None;
            return;
        };
        let mut status = json!({});
        game.describe(&mut status);
        format!("{}|{}|{}", status["dimension"], status["gameMode"], status["dead"])
    };
    let now = *bot.ticks.borrow();
    let due = match bot.reported.borrow().as_ref() {
        Some((reported, at)) => *reported != seen || now.saturating_sub(*at) >= REFRESH_TICKS,
        None => true,
    };
    if due {
        *bot.reported.borrow_mut() = Some((seen, now));
        bot.status("ready", None);
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
                task_pool_options: TaskPoolOptions {
                    min_total_threads: 1,
                    max_total_threads: 1,
                    ..Default::default()
                },
            }),
            DefaultBotPlugins
                .build()
                .disable::<AutoRespawnPlugin>()
                .disable::<AutoReconnectPlugin>(),
            DefaultSwarmPlugins,
            HudPlugin,
            MenusPlugin,
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
    torn_down(&bot).await;
    bot.status("connecting", None);

    let address = format!("{host}:{port}");
    let resolved = address
        .clone()
        .resolve()
        .await
        .map_err(|error| Failure::refused("JOIN_FAILED_DIAL", format!("{address} did not resolve: {error}")))?;

    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let client = start(
        Account::offline(&username),
        ConnectOpts {
            address: resolved,
            server_proxy: None,
            sessionserver_proxy: None,
        },
    )
    .await;
    let (events, receiver) = mpsc::unbounded_channel();
    let (packets, hud_packets) = mpsc::unbounded_channel();
    /*
    azalea keeps one entity per name and never takes the profile a login left on it, so the one
    from the connection before is taken off here: the pump reads the profile's presence as this
    connection's login having finished.
    */
    ecs()
        .write()
        .entity_mut(client.entity)
        .remove::<GameProfileComponent>()
        .insert((
            LocalPlayerEvents(events),
            HudPackets(packets),
            Menus::default(),
            Known::default(),
        ));

    let (alive, ended) = oneshot::channel();
    *bot.game.borrow_mut() = Some(Game {
        generation,
        client,
        address: address.clone(),
        username: username.clone(),
        spawned: Cell::new(false),
        leaving: Cell::new(false),
        cause_of_death: RefCell::new(None),
        hud: RefCell::new(Hud::default()),
        ended,
    });

    let (joined, outcome) = oneshot::channel();
    tokio::task::spawn_local(pump(bot.clone(), generation, receiver, hud_packets, joined, alive));

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

    let entity = entity
        .recv()
        .await
        .expect("azalea drops the join callback only when its ECS has stopped");
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
    let Some(game) = bot.game.borrow_mut().take() else {
        return;
    };
    game.leaving.set(true);
    game.client.disconnect();
    feeds::close_all(bot);
    *bot.departing.borrow_mut() = Some(Departing {
        client: game.client,
        ended: game.ended,
    });
}

/// Wait until the connection the last leave ended is gone from azalea: its pump has seen the
/// disconnect, and the entity no longer holds a connection a join would be refused for.
///
/// The pump ending is the disconnect event, which azalea sends from the frame that removes the
/// connection; the component check covers the event arriving before that frame has applied it.
async fn torn_down(bot: &Bot) {
    let Some(Departing { client, ended }) = bot.departing.borrow_mut().take() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + TEARDOWN;

    let _ = tokio::time::timeout_at(deadline, ended).await;
    while client.get_component::<RawConnection>().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Everything azalea says about one connection, until it ends.
async fn pump(
    bot: Rc<Bot>,
    generation: u64,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut packets: mpsc::UnboundedReceiver<Arc<ClientboundGamePacket>>,
    joined: oneshot::Sender<Result<(), Failure>>,
    _alive: oneshot::Sender<()>,
) {
    let mut joined = Some(joined);
    let mut logged_in = false;
    let mine = |bot: &Bot| {
        bot.game
            .borrow()
            .as_ref()
            .is_some_and(|game| game.generation == generation)
    };

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
            Some(packet) = packets.recv() => {
                if let Some(game) = bot.game.borrow().as_ref().filter(|game| game.generation == generation) {
                    crate::tools::received(&bot, &game.client, &packet);
                }
                if mine(&bot) {
                    hud::apply(&bot, &packet);
                }
                continue;
            }
        };
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
            Event::Tick => {
                bot.ticks.send_modify(|ticks| *ticks += 1);
                /*
                On the tick rather than on a timer of its own: the server sets the cadence, which
                a one-second timer could not keep below a second, and the task is awake for the
                tick already.
                */
                feeds::flush(&bot);
                notice_world(&bot, generation);
            }
            Event::Death(kill) => {
                if let Some(game) = bot.game.borrow().as_ref().filter(|game| game.generation == generation) {
                    *game.cause_of_death.borrow_mut() = kill.map(|kill| kill.message.to_string());
                }
                /*
                Death does not end the connection, so the state stays ready and the status goes again
                with dead set: a status that only moved with the state told a caller a dead bot was
                ready to walk.
                */
                bot.status("ready", None);
            }
            Event::Chat(packet) => feeds::chat(&bot, &packet),
            Event::ConnectionFailed(error) => {
                if let Some(joined) = joined.take() {
                    let _ = joined.send(Err(Failure::refused("JOIN_FAILED_DIAL", error.to_string())));
                }
                break;
            }
            Event::Disconnect(reason) => {
                let reason = reason
                    .map(|text| text.to_string())
                    .unwrap_or_else(|| "disconnected".into());
                let dropped = mine(&bot) && !bot.game.borrow().as_ref().is_some_and(|game| game.leaving.get());

                if let Some(joined) = joined.take() {
                    /*
                    Kicked before the login finished is a login refused; after it, a world that
                    never came. The Login event is the play-state login packet, which comes after
                    configuration, so a server that took the login and kicked from configuration
                    -- a plugin, a transfer -- has sent no event that says so. The profile the
                    login handshake put on the entity says it, and a disconnect leaves it there.
                    */
                    let profile = bot
                        .game
                        .borrow()
                        .as_ref()
                        .filter(|game| game.generation == generation)
                        .is_some_and(|game| game.client.get_component::<GameProfileComponent>().is_some());
                    let code = if logged_in || profile {
                        "JOIN_FAILED_SPAWN"
                    } else {
                        "JOIN_FAILED_LOGIN"
                    };
                    let _ = joined.send(Err(Failure::refused(code, reason.clone())));
                } else if dropped {
                    bot.status("disconnected", Some(&reason));
                }
                if mine(&bot) {
                    bot.game.borrow_mut().take();
                    feeds::close_all(&bot);
                }
                info!("disconnected at {}: {reason}", now_millis());
                break;
            }
            _ => {}
        }
    }
}
