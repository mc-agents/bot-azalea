use std::str::FromStr;

use azalea::BlockPos;
use azalea::registry::Registry;
use azalea::registry::builtin::BlockKind;
use serde_json::json;

use super::args::{plain, point, position, text};
use super::{Tool, in_world};
use crate::calls::{Answer, Failure};

pub const GET_BLOCK_INFO: Tool = Tool {
    name: "get-block-info",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;
            let state = in_world(&bot, |game| game.client.world().read().get_block_state(at))?;

            /*
            Outside the loaded chunks there is nothing to say about the block, which is not air. The
            name goes without its namespace, as the other kind of bot sends it: the server words the
            DTO as it arrives, and "minecraft:dirt" from one kind is a second answer to one block.
            */
            let block = state.map(|state| {
                let kind = BlockKind::from(state);
                json!({"name": plain(kind.to_str()), "type": kind.to_u32(), "position": point(at)})
            });
            Ok(Answer::data(format!("block at {}, {}, {}", at.x, at.y, at.z), json!({"position": point(at), "block": block})))
        })
    },
};

pub const FIND_BLOCKS: Tool = Tool {
    name: "find-blocks",
    run: |bot, args| {
        Box::pin(async move {
            let asked = text(&args, "blockType")?.to_owned();
            let max_distance = args["maxDistance"].as_f64().unwrap_or(16.0);
            let count = args["count"].as_u64().unwrap_or(1) as usize;

            /*
            An unknown name is refused. Looking it up and taking the registry's default -- which is
            air -- answers a typo with every empty block in range.
            */
            let wanted = BlockKind::from_str(plain(&asked))
                .map_err(|_| Failure::refused("NO_SUCH_BLOCK", format!("there is no block called {asked}")))?;

            let found = in_world(&bot, |game| {
                let from = BlockPos::from(game.client.position());
                let world = game.client.world();
                let world = world.read();
                let radius = max_distance.ceil() as i32;
                let limit = max_distance * max_distance;
                let mut found = Vec::new();

                for x in -radius..=radius {
                    for y in -radius..=radius {
                        for z in -radius..=radius {
                            if (x * x + y * y + z * z) as f64 > limit {
                                continue;
                            }
                            let at = BlockPos::new(from.x + x, from.y + y, from.z + z);
                            if world.get_block_state(at).is_some_and(|state| BlockKind::from(state) == wanted) {
                                found.push(at);
                            }
                        }
                    }
                }

                /* Nearest first, and a fixed order among blocks at the same distance so two runs agree. */
                let distance = |at: &BlockPos| {
                    let (dx, dy, dz) = (at.x - from.x, at.y - from.y, at.z - from.z);
                    dx * dx + dy * dy + dz * dz
                };
                found.sort_by_key(|at| (distance(at), at.x, at.y, at.z));
                found.truncate(count);
                found
            })?;

            Ok(Answer::data(
                format!("found {} {asked}", found.len()),
                json!({
                    "blockType": asked,
                    "maxDistance": max_distance,
                    "positions": found.into_iter().map(point).collect::<Vec<_>>(),
                }),
            ))
        })
    },
};
