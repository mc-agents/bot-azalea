use azalea::entity::Dead;
use azalea::entity::metadata::Sprinting;
use azalea::respawn::PerformRespawnEvent;
use azalea::world::WorldName;
use azalea::{BlockPos, Client, PhysicsState, WalkDirection};
use serde_json::Value;

use super::args::{position, text, written};
use super::{Tool, alive, in_world, tick};
use crate::calls::{Answer, Failure};

/// How long a tick is, for a duration the caller gave in milliseconds.
const TICK_MS: u64 = 50;

/// The position the server sends lands after the respawn itself, and it is the one worth reporting.
const SETTLE_TICKS: u32 = 20;

pub const LOOK_AT: Tool = Tool {
    name: "look-at",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;
            /* At the middle of the block: its corner puts the thing being looked at half out of view. */
            alive(&bot, |game| game.client.look_at(at.center()))?;
            Ok(Answer::text(format!("Looking at {}.", written(at))))
        })
    },
};

pub const JUMP: Tool = Tool {
    name: "jump",
    run: |bot, _args| {
        Box::pin(async move {
            /* Physics only jumps from the ground, which is also all a server would believe. */
            alive(&bot, |game| game.client.jump())?;
            Ok(Answer::text("Jumped."))
        })
    },
};

pub const SET_STANCE: Tool = Tool {
    name: "set-stance",
    run: |bot, args| {
        Box::pin(async move {
            let wanted = |name: &str| match &args[name] {
                Value::Null => Ok(None),
                Value::Bool(wanted) => Ok(Some(*wanted)),
                _ => Err(Failure::bad_args(format!("expected a boolean or null for {name}"))),
            };
            let (sneak, sprint) = (wanted("sneak")?, wanted("sprint")?);

            /*
            A field left null is left alone: "start sneaking" must not quietly stop a sprint that was
            already running. These are the keys held, which is what the server is told every tick;
            whether the sprint has actually started is the game's to decide once the bot moves.
            */
            let (sneaking, sprinting) = alive(&bot, |game| {
                let client = &game.client;
                if let Some(sneak) = sneak {
                    client.set_crouching(sneak);
                }
                match sprint {
                    Some(true) => client.query_self::<&mut PhysicsState, _>(|mut state| state.trying_to_sprint = true),
                    /* Through a walk, because that is what takes the sprint's speed back off. */
                    Some(false) => client.walk(client.query_self::<&PhysicsState, _>(|state| state.move_direction)),
                    None => {}
                }
                let sprinting = sprint.unwrap_or_else(|| {
                    client.query_self::<&PhysicsState, _>(|state| state.trying_to_sprint)
                        || client.get_component::<Sprinting>().is_some_and(|sprinting| sprinting.0)
                });
                (client.crouching(), sprinting)
            })?;

            Ok(Answer::text(format!("sneaking: {sneaking}, sprinting: {sprinting}")))
        })
    },
};

pub const MOVE_IN_DIRECTION: Tool = Tool {
    name: "move-in-direction",
    run: |bot, args| {
        Box::pin(async move {
            let direction = text(&args, "direction")?.to_owned();
            let key = match direction.as_str() {
                "forward" => WalkDirection::Forward,
                "back" => WalkDirection::Backward,
                "left" => WalkDirection::Left,
                "right" => WalkDirection::Right,
                other => return Err(Failure::bad_args(format!("unknown direction {other}"))),
            };
            let duration = args["durationMs"].as_u64().unwrap_or(1000);

            let held = Held(alive(&bot, |game| game.client.clone())?);
            for _ in 0..duration.div_ceil(TICK_MS) {
                alive(&bot, |_| held.press(key))?;
                tick(&bot).await;
            }
            drop(held);

            Ok(Answer::text(format!("Moved {direction} for {duration}ms.")))
        })
    },
};

/// A movement key, let go of when the call ends however it ends. A cancelled hold that left the key
/// down would walk the bot away for good.
///
/// The key is set on the physics state rather than through a walk, which would also drop a sprint
/// set-stance asked for.
struct Held(Client);

impl Held {
    fn press(&self, key: WalkDirection) {
        self.0.query_self::<&mut PhysicsState, _>(|mut state| state.move_direction = key);
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.0.get_component::<PhysicsState>().is_some() {
            self.press(WalkDirection::None);
        }
    }
}

pub const RESPAWN: Tool = Tool {
    name: "respawn",
    run: |bot, _args| {
        Box::pin(async move {
            /*
            Never done unasked. A server under test may be checking what happens on death -- a kept
            inventory, where it sends a player back to -- and a bot that got up by itself would have
            walked through the one thing the check was looking at.
            */
            let dead = in_world(&bot, |game| {
                let client = &game.client;
                let dead = client.get_component::<Dead>().is_some();
                if dead {
                    client.ecs.write().write_message(PerformRespawnEvent { entity: client.entity });
                }
                (dead, BlockPos::from(client.position()))
            })?;
            if let (false, at) = dead {
                return Ok(Answer::text(format!("The bot is not dead. It is at {}.", written(at))));
            }

            /*
            Pressing the button is one packet, and answering then would say where the body fell. The
            server answers with a respawn, which is when the client stops being dead, and then with
            where to stand.
            */
            while in_world(&bot, |game| game.client.get_component::<Dead>().is_some())? {
                tick(&bot).await;
            }
            for _ in 0..SETTLE_TICKS {
                tick(&bot).await;
            }

            let (at, dimension) = in_world(&bot, |game| {
                game.forget_death();
                let client = &game.client;
                (
                    BlockPos::from(client.position()),
                    client.get_component::<WorldName>().map(|name| name.0.path().to_owned()).unwrap_or_default(),
                )
            })?;
            bot.status("ready", None);
            Ok(Answer::text(format!("Respawned at {} in {dimension}.", written(at))))
        })
    },
};
