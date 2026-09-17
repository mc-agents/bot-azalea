use azalea::core::entity_id::MinecraftEntityId;
use azalea::entity::indexing::EntityIdIndex;
use azalea::entity::metadata::{AirSupply, Health};
use azalea::entity::{Dead, EntityKindComponent};
use azalea::local_player::LocalGameMode;
use azalea::world::WorldName;
use serde_json::{Value, json};

use super::args::{plain, point};
use super::{Tool, in_world};
use crate::calls::Answer;
use crate::game::Game;

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
                    "vehicle": vehicle(game),
                })
            })?;
            Ok(Answer::data("player state", data))
        })
    },
};

/// The bottom of the stack the bot rides, and the entity it actually sits on when that is another
/// one. A plugin seats a player on an invisible entity riding the mount, and either one alone
/// describes half of it.
fn vehicle(game: &Game) -> Value {
    let client = &game.client;
    let Some(me) = client.get_component::<MinecraftEntityId>().map(|id| id.0) else {
        return Value::Null;
    };

    /* Each read takes the ECS lock and lets it go, so none is held while the next is taken. */
    let loaded = |id: i32| {
        client
            .get_component::<EntityIdIndex>()
            .and_then(|index| index.get_by_minecraft_entity(MinecraftEntityId(id)))
    };
    let kind = |id: i32| {
        let entity = loaded(id)?;
        client
            .get_entity_component::<EntityKindComponent>(entity)
            .map(|kind| plain(kind.0.to_str()))
    };

    let hud = game.hud.borrow();
    let Some(seat) = hud.vehicle_of(me).filter(|id| loaded(*id).is_some()) else {
        return Value::Null;
    };
    let mut root = seat;
    /* Bounded, because a stack the server described inconsistently must not hang the call. */
    for _ in 0..16 {
        match hud.vehicle_of(root).filter(|id| loaded(*id).is_some()) {
            Some(below) => root = below,
            None => break,
        }
    }

    let ridden = |id: i32| json!({"type": kind(id).unwrap_or_default(), "id": id});
    let mut vehicle = ridden(root);
    vehicle["seat"] = if seat == root { Value::Null } else { ridden(seat) };
    vehicle
}

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
