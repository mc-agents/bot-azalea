use std::time::Duration;

use azalea::BlockPos;
use azalea::protocol::packets::game::ServerboundCommandSuggestion;
use azalea::world::WorldName;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use super::args::text;
use super::{Tool, in_world};
use crate::calls::{Answer, Failure};

/// Send a slash command as the bot.
///
/// Sending is the whole of the bot's half. What the server says back is chat, and the buffer that
/// catches it is mcp-server's, which marks it before the call and reads past the mark afterwards.
///
/// The command goes unsigned, because azalea signs nothing but chat. A server in online mode that
/// enforces secure profiles does not run a command with a signed argument -- /msg, /say, /me -- sent
/// that way, and the reply collected is the server saying the signature was invalid.
pub const RUN_COMMAND: Tool = Tool {
    name: "run-command",
    run: |bot, args| {
        Box::pin(async move {
            let command = text(&args, "command")?;
            let slashed = slashed(command);

            in_world(&bot, |game| game.client.write_command_packet(&slashed[1..]))?;
            Ok(Answer::text(format!("Ran {slashed}.")))
        })
    },
};

/// What completes a partial command, which is how to find out what a plugin offers without asking.
///
/// Asked of the server, which is where a vanilla client gets the arguments it cannot complete
/// alone. azalea reads the command tree the server sends at login into nodes it cannot suggest
/// from, so there is no tree here to answer from.
pub const COMPLETE_COMMAND: Tool = Tool {
    name: "complete-command",
    run: |bot, args| {
        Box::pin(async move {
            let asked = text(&args, "text")?.to_owned();
            let limit = args["limit"].as_u64().unwrap_or(60) as usize;
            let timeout = Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(5_000));

            let (id, answered) = in_world(&bot, |game| {
                let (id, answered) = game.hud.borrow_mut().ask();
                game.client.write_packet(ServerboundCommandSuggestion {
                    id,
                    command: slashed(&asked),
                });
                (id, answered)
            })?;

            let suggestions = match tokio::time::timeout(timeout, answered).await {
                Ok(Ok(suggestions)) => suggestions,
                /* Dropped unanswered: the connection that asked is gone. */
                Ok(Err(_)) => return Err(Failure::not_in_game()),
                Err(_) => {
                    if let Some(game) = bot.game.borrow().as_ref() {
                        game.hud.borrow_mut().forget(id);
                    }
                    return Err(Failure::refused(
                        "COMPLETION_TIMEOUT",
                        format!(
                            "the server did not answer what completes {asked} within {}ms",
                            timeout.as_millis()
                        ),
                    ));
                }
            };

            /*
            Case-insensitively, as brigadier orders them. azalea sorts what it reads by case, which
            puts every plugin command with a capital in it ahead of the rest.
            */
            let mut offered = suggestions.list().to_vec();
            offered.sort_by_cached_key(|suggestion| suggestion.text().to_lowercase());

            let completions: Vec<_> = offered
                .iter()
                .take(limit)
                .map(|suggestion| json!({"name": suggestion.text(), "tooltip": suggestion.tooltip}))
                .collect();

            Ok(Answer::data(
                "complete-command",
                json!({"text": asked, "total": offered.len(), "completions": completions}),
            ))
        })
    },
};

/// Send the bot to another backend through the proxy and wait until it is there.
///
/// A backend switch is a fresh login on the same connection: the proxy takes the client back to
/// configuration and logs it into the next server, and the socket never closes. So the next login
/// is what says the bot arrived.
///
/// The other outcome is the proxy answering in chat, and it arrives first when it happens. There is
/// no packet for "that server does not exist" -- a proxy says it the way it would say anything else
/// -- so the two are waited on together and whichever lands first decides.
pub const SWITCH_SERVER: Tool = Tool {
    name: "switch-server",
    run: |bot, args| {
        Box::pin(async move {
            let target = text(&args, "target")?.to_owned();
            let timeout = Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(30_000));

            let mut heard = bot.heard.subscribe();
            let logins = in_world(&bot, |game| {
                game.client.write_command_packet(&format!("server {target}"));
                game.hud.borrow().logins
            })?;

            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            let timed_out = || {
                Failure::refused(
                    "SWITCH_TIMEOUT",
                    format!(
                        "The bot did not arrive on \"{target}\" within {}ms. Check read-chat for what the proxy said and get-bot-status for the connection state.",
                        timeout.as_millis()
                    ),
                )
            };

            /* Polled rather than woken: through configuration there is no world, and no ticks to wake on. */
            let mut poll = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    line = heard.recv() => match line {
                        Ok(line) if already_there(&line) => {
                            let at = in_world(&bot, where_is)?;
                            return Ok(Answer::text(format!("Already on \"{target}\" at {}.", at.0)));
                        }
                        Ok(line) if refused(&line) => {
                            return Err(Failure::refused("SWITCH_REFUSED", format!("The proxy refused the switch: {line}")));
                        }
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return Err(Failure::not_in_game()),
                    },
                    _ = poll.tick() => {
                        let arrived = bot
                            .game
                            .borrow()
                            .as_ref()
                            .is_some_and(|game| game.spawned() && game.hud.borrow().logins > logins);
                        if arrived {
                            break;
                        }
                    }
                    _ = &mut deadline => return Err(timed_out()),
                }
            }

            /* The position the backend sends lands after the login, and it is the one worth reporting. */
            let mut ticks = bot.ticks.subscribe();
            let settled = *ticks.borrow_and_update() + SETTLE_TICKS;
            while *ticks.borrow_and_update() < settled {
                tokio::select! {
                    changed = ticks.changed() => if changed.is_err() { return Err(Failure::not_in_game()) },
                    _ = &mut deadline => return Err(timed_out()),
                }
            }

            let (at, dimension) = in_world(&bot, where_is)?;
            Ok(Answer::text(format!("Now on \"{target}\" at {at} in {dimension}.")))
        })
    },
};

const SETTLE_TICKS: u64 = 20;

pub(super) fn slashed(command: &str) -> String {
    if command.starts_with('/') {
        command.to_owned()
    } else {
        format!("/{command}")
    }
}

fn where_is(game: &crate::game::Game) -> (String, String) {
    let at = BlockPos::from(game.client.position());
    let dimension = game
        .client
        .get_component::<WorldName>()
        .map(|name| name.0.path().to_owned())
        .unwrap_or_default();
    (format!("({}, {}, {})", at.x, at.y, at.z), dimension)
}

/// What Velocity and BungeeCord say when the bot is already where it was sent.
fn already_there(line: &str) -> bool {
    line.to_lowercase().contains("already connected")
}

/// What they say when there is nowhere to send it.
fn refused(line: &str) -> bool {
    let line = line.to_lowercase();
    [
        "does not exist",
        "doesn't exist",
        "doesnot exist",
        "does n't exist",
        "unable to connect",
        "no available server",
        "not a valid server",
    ]
    .iter()
    .any(|said| line.contains(said))
}
