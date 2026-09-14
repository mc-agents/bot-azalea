use azalea::pathfinder::goals::RadiusGoal;
use azalea::pathfinder::{PathfinderClientExt, PathfinderOpts};
use azalea::{Client, Vec3};

use super::alive;
use super::text::one_decimal;
use crate::bot::Bot;
use crate::calls::Failure;

/// Feet to block corner, or feet to feet for an entity: the other kind of bot's reach, measured the
/// same way, so a tool walks to the same spot on either.
const REACH: f64 = 3.0;

/// How close the pathfinder is sent. Its goal is measured from the middle of the block the bot
/// stands in, which is half a block above the feet, so a goal of the full reach can end outside it.
const GOAL_RADIUS: f32 = 2.0;

/// A target that moved further than this from where the pathfinder was sent is sent again.
const FOLLOW: f64 = 1.0;

/// Ticks before a route that ended short is searched again. A search takes at least a second.
const REPLAN_TICKS: u32 = 20;

/// Under this in a tick of walking is not walking, and this many ticks of it is being stuck.
const PROGRESS: f64 = 0.2;
const PATIENCE_TICKS: u32 = 40;

/// Getting close enough to touch something, asked once a tick.
///
/// The walking is azalea's pathfinder, told not to dig: a bot that tunnelled to a chest would change
/// the world a check is about. Dropped with the call, it stops the walk, since a call that lost its
/// race to a deadline or a cancel would otherwise leave the bot walking on its own.
pub struct Approach {
    walking: Option<(Client, Vec3)>,
    since_sent: u32,
    ended_short: bool,
    noted: Option<Vec3>,
    stuck_ticks: u32,
}

impl Approach {
    pub fn new() -> Approach {
        Approach { walking: None, since_sent: 0, ended_short: false, noted: None, stuck_ticks: 0 }
    }

    pub fn reached(&mut self, bot: &Bot, target: Vec3, what: &str) -> Result<bool, Failure> {
        alive(bot, |game| self.step(&game.client, target, what))?
    }

    fn step(&mut self, client: &Client, target: Vec3, what: &str) -> Result<bool, Failure> {
        let at = client.position();
        if at.distance_to(target) <= REACH {
            self.stop();
            return Ok(true);
        }

        self.since_sent += 1;
        let idle = client.is_goto_target_reached();
        let moved = self.walking.as_ref().is_none_or(|(_, goal)| goal.distance_to(target) > FOLLOW);

        if moved || idle && self.since_sent >= REPLAN_TICKS {
            self.ended_short |= !moved;
            client.start_goto_with_opts(
                RadiusGoal::new(target, GOAL_RADIUS),
                PathfinderOpts::new().allow_mining(false).retry_on_no_path(false),
            );
            self.walking = Some((client.clone(), target));
            self.since_sent = 0;
        }

        /* A search is not being stuck: the bot stands still for it by design. */
        if client.is_calculating_path() {
            return Ok(false);
        }
        if self.noted.is_none_or(|noted| at.distance_to(noted) > PROGRESS) {
            self.noted = Some(at);
            self.stuck_ticks = 0;
            return Ok(false);
        }

        self.stuck_ticks += 1;
        if self.stuck_ticks <= PATIENCE_TICKS {
            return Ok(false);
        }

        self.stop();
        let trouble = if self.ended_short || idle {
            "no route to it over the blocks the client can see"
        } else {
            "the route it was following stopped getting closer"
        };
        Err(Failure::refused(
            "UNREACHABLE",
            format!(
                "could not get within reach of {what}; it stopped {} blocks away: {trouble}.",
                one_decimal(at.distance_to(target))
            ),
        ))
    }

    fn stop(&mut self) {
        /* A connection that ended took the entity with it, and a stop sent to an entity that is gone
        fails inside the ECS every connection shares. */
        if let Some((client, _)) = self.walking.take()
            && client.ecs.read().get_entity(client.entity).is_ok()
        {
            client.force_stop_pathfinding();
        }
    }
}

impl Drop for Approach {
    fn drop(&mut self) {
        self.stop();
    }
}
