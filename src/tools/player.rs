use azalea::entity::Dead;
use azalea::entity::metadata::{AirSupply, Health};
use azalea::local_player::LocalGameMode;
use azalea::world::WorldName;
use serde_json::{Value, json};

use super::args::point;
use super::{Tool, in_world};
use crate::calls::Answer;

/// A full breath. Below it the bot is spending air, and only then is it worth saying how much.
const FULL_AIR: i32 = 300;

pub const GET_PLAYER_STATE: Tool = Tool {
    name: "get-player-state",
    run: |bot, _args| {
        Box::pin(async move {
            let data = in_world(&bot, |game| {
                let client = &game.client;
                let hunger = client.hunger();
                let experience = client.experience();
                let air = client.get_component::<AirSupply>().map(|air| air.0).unwrap_or(FULL_AIR);

                json!({
                    "health": client.get_component::<Health>().map(|health| health.0).unwrap_or_default(),
                    "food": hunger.food,
                    "saturation": hunger.saturation,
                    "experience": {"level": experience.level, "progress": experience.progress, "points": experience.total},
                    "gameMode": game_mode(client.get_component::<LocalGameMode>().map(|mode| mode.current)),
                    "dimension": client.get_component::<WorldName>().map(|name| name.0.path().to_owned()).unwrap_or_default(),
                    "position": point(azalea::BlockPos::from(client.position())),
                    "oxygen": if air < FULL_AIR { json!(air) } else { Value::Null },
                    "dead": client.get_component::<Dead>().is_some(),
                    "causeOfDeath": game.cause_of_death(),
                    "causeOfDeathComponent": Value::Null,
                })
            })?;
            Ok(Answer::data("player state", data))
        })
    },
};

/// The game's own lower-case name for a mode, which is what the other kind of bot sends.
pub fn game_mode(mode: Option<azalea::core::game_type::GameMode>) -> &'static str {
    use azalea::core::game_type::GameMode;
    match mode {
        Some(GameMode::Survival) => "survival",
        Some(GameMode::Creative) => "creative",
        Some(GameMode::Adventure) => "adventure",
        Some(GameMode::Spectator) => "spectator",
        None => "",
    }
}
