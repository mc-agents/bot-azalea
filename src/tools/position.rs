use serde_json::json;

use super::{Tool, in_world};
use crate::calls::Answer;

pub const GET_POSITION: Tool = Tool {
    name: "get-position",
    run: |bot, _args| {
        Box::pin(async move {
            /*
            The block the player is in, floored. Which block an entity occupies is game knowledge, and
            truncating a negative coordinate towards zero names the block next door.
            */
            let (x, y, z) = in_world(&bot, |game| {
                let position = game.client.position();
                (position.x.floor() as i32, position.y.floor() as i32, position.z.floor() as i32)
            })?;
            Ok(Answer::data(format!("standing at {x}, {y}, {z}"), json!({"position": {"x": x, "y": y, "z": z}})))
        })
    },
};
