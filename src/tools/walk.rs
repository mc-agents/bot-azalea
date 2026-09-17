use std::time::{Duration, Instant};

use azalea::pathfinder::goals::RadiusGoal;
use azalea::pathfinder::{ExecutingPath, PathfinderClientExt, PathfinderOpts};
use azalea::{BlockPos, Client, Vec3};

use super::args::{position, written};
use super::text::{one_decimal, plain};
use super::{Tool, alive, tick};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};

/// Under this in [`PATIENCE_TICKS`] of walking is not walking. Measured against the last place the
/// bot got to rather than the last tick, because a bot pressed against a wall still twitches by a
/// fraction of a block every tick and would never look stuck.
const PROGRESS: f64 = 0.2;
const PATIENCE_TICKS: u32 = 40;

pub const MOVE_TO_POSITION: Tool = Tool {
    name: "move-to-position",
    run: |bot, args| {
        Box::pin(async move {
            let target = position(&args)?;
            let range = args["range"]
                .as_f64()
                .ok_or_else(|| Failure::bad_args("expected a number for range"))?;
            let timeout = Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(60_000));

            /*
            A target the way cannot reach is reported as how far the bot got rather than waited out:
            a caller who hears "stopped 6 blocks short" can teleport, and one who hears nothing for
            a minute cannot.
            */
            let started = Instant::now();
            let mut walk = Walk::new(&bot, target, range)?;
            loop {
                match walk.step(&bot)? {
                    Stride::Arrived => {
                        return Ok(Answer::text(format!(
                            "Moved to within {} block(s) of {}.",
                            plain(range),
                            written(target)
                        )));
                    }
                    Stride::Stuck(distance) => return Err(unreachable(target, timeout, distance, &walk)),
                    Stride::Walking(distance) if started.elapsed() > timeout => {
                        return Err(unreachable(target, timeout, distance, &walk));
                    }
                    Stride::Walking(_) => tick(&bot).await,
                }
            }
        })
    },
};

fn unreachable(target: BlockPos, timeout: Duration, distance: f64, walk: &Walk) -> Failure {
    Failure::refused(
        "UNREACHABLE",
        format!(
            "could not reach {} within {}ms; it stopped {} blocks away: {}. Teleport with run-command when the way is not walkable.",
            written(target),
            timeout.as_millis(),
            one_decimal(distance),
            walk.trouble()
        ),
    )
}

enum Stride {
    Arrived,
    /// How far from the target, when it was not reached.
    Walking(f64),
    Stuck(f64),
}

/// azalea's pathfinder, held to the caller's range of a block's corner and stopped when this is
/// dropped, the way [`super::approach::Approach`] is for a tool's fixed reach. This one keeps the
/// route it was given, because the refusal says how far along it the bot got.
struct Walk {
    client: Client,
    corner: Vec3,
    target: BlockPos,
    range: f64,
    started: bool,
    loaded: bool,
    /// The longest route the pathfinder has followed so far, and how much of it is left.
    route: usize,
    left: usize,
    noted: Option<Vec3>,
    still: u32,
}

impl Walk {
    fn new(bot: &Bot, target: BlockPos, range: f64) -> Result<Walk, Failure> {
        let client = alive(bot, |game| game.client.clone())?;
        Ok(Walk {
            client,
            corner: Vec3::new(target.x as f64, target.y as f64, target.z as f64),
            target,
            range,
            started: false,
            loaded: true,
            route: 0,
            left: 0,
            noted: None,
            still: 0,
        })
    }

    fn step(&mut self, bot: &Bot) -> Result<Stride, Failure> {
        alive(bot, |_| ())?;
        let here = self.client.position();
        let distance = here.distance_to(self.corner);

        if distance <= self.range {
            self.stop();
            return Ok(Stride::Arrived);
        }

        /*
        A target outside the chunks the client has is not unreachable, it is unknown, and the
        pathfinder would set off towards it anyway over what it takes for solid ground.
        */
        self.loaded = {
            let world = self.client.world();
            let world = world.read();
            world.get_block_state(self.target).is_some() && world.get_block_state(BlockPos::from(here)).is_some()
        };
        if self.loaded && !self.started {
            self.started = true;
            /*
            The node a goal is tested on is the block the feet are in, and its centre is half a
            block above the feet: raised by the same half, the radius measures feet to corner.
            Nothing is broken on the way and a search that finds nothing is not retried, because a
            player walks round a wall rather than through it, and a search retried forever never
            lets the bot stand still long enough to be called stuck.
            */
            self.client.start_goto_with_opts(
                RadiusGoal::new(self.corner.up(0.5), self.range as f32),
                PathfinderOpts::new().allow_mining(false).retry_on_no_path(false),
            );
        }

        let left = self
            .client
            .get_component::<ExecutingPath>()
            .map_or(0, |path| path.path.len());
        if left > self.left {
            self.route = self.route.max(left);
        }
        self.left = left;

        /* A search takes up to seconds on its own thread, and standing still through it is not being stuck. */
        if self.client.is_calculating_path() {
            self.still = 0;
            return Ok(Stride::Walking(distance));
        }
        match self.noted {
            Some(noted) if here.distance_to(noted) <= PROGRESS => self.still += 1,
            _ => {
                self.noted = Some(here);
                self.still = 0;
            }
        }
        if self.still > PATIENCE_TICKS {
            self.stop();
            return Ok(Stride::Stuck(distance));
        }
        Ok(Stride::Walking(distance))
    }

    /// Why the walk is not getting anywhere. "No route" and "the route ran out" send a caller to
    /// different places: the first is a world the client cannot see through, the second a way that
    /// exists and stops short.
    fn trouble(&self) -> String {
        if !self.loaded {
            "the client could not search a route: the target or the ground under this bot is outside the chunks it has"
                .into()
        } else if self.route == 0 {
            "no route to it over the blocks the client can see".into()
        } else {
            format!(
                "followed a route of {} block(s) and stopped at step {}",
                self.route,
                self.route - self.left
            )
        }
    }

    fn stop(&mut self) {
        /* A connection that ended took the entity with it, and a stop sent to it fails in the shared ECS. */
        if std::mem::take(&mut self.started) && self.client.ecs.read().get_entity(self.client.entity).is_ok() {
            self.client.force_stop_pathfinding();
        }
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        self.stop();
    }
}
