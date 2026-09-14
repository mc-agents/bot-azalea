use azalea::Identifier;
use azalea::core::data_registry::ResolvableDataRegistry;
use serde_json::{Value, json};
use simdnbt::owned::NbtTag;

use super::{Tool, in_world};
use crate::calls::Answer;

const DAY: u64 = 24_000;

/// The day timeline's sky light, as 26.1.2's `minecraft:day` keys it: a multiplier on full light at
/// a tick of the day, eased linearly between keys and round the end of the day back to the first.
const DAYLIGHT: [(u64, f32); 4] = [(133, 1.0), (11_867, 1.0), (13_670, 0.266_666_68), (22_330, 0.266_666_68)];

/// What rain and thunder blend the sky light towards, and how far at full strength.
const OVERCAST: f32 = 4.0;
const RAIN_WEIGHT: f32 = 0.3125;
const THUNDER_WEIGHT: f32 = 0.527_343_75;

/// The clock the world ticks on, for a feature that only happens at a certain time of day.
///
/// Not whatever a server draws on its own HUD: that is a scoreboard or an action bar, and it can
/// say anything it likes.
pub const GET_WORLD_STATE: Tool = Tool {
    name: "get-world-state",
    run: |bot, _args| {
        Box::pin(async move {
            let data = in_world(&bot, |game| {
                let hud = game.hud.borrow();
                let ticks_now = *bot.ticks.borrow();

                /*
                The dimension type says which clock is the default, whether time stands still and
                whether weather can happen at all. azalea parses the type into a struct that keeps
                none of those, and leaves them in its bag of fields it did not model.
                */
                let sky = hud.dimension().and_then(|(kind, name)| {
                    game.client.with_registry_holder(|registries| {
                        let (_, element) = kind.resolve(registries)?;
                        let flag = |field: &str| matches!(element._extra.get(field), Some(NbtTag::Byte(value)) if *value != 0);

                        let clock = match element._extra.get("default_clock") {
                            Some(NbtTag::String(clock)) => registries
                                .extra
                                .get(&Identifier::new("world_clock"))
                                .and_then(|clocks| clocks.map.get_index_of(&Identifier::new(clock.to_str())))
                                .map(|index| index as u32),
                            _ => None,
                        };

                        Some(Sky {
                            clock,
                            fixed_time: flag("has_fixed_time"),
                            weather: flag("has_skylight") && !flag("has_ceiling") && name.to_string() != "minecraft:the_end",
                        })
                    })
                });
                let sky = sky.unwrap_or(Sky { clock: None, fixed_time: false, weather: false });

                /* A dimension with no clock of its own reads zero, as the game's own lookup does. */
                let time = sky.clock.and_then(|clock| hud.clock_ticks(clock, ticks_now)).unwrap_or(0);
                let rain = if sky.weather { hud.raining() } else { 0.0 };
                let thunder = if sky.weather { hud.thundering() } else { 0.0 };

                let weather = if thunder > 0.9 {
                    "thunder"
                } else if rain > 0.2 {
                    "rain"
                } else {
                    "clear"
                };

                /* The client calls it day while the sky is darkened by less than four levels. */
                let bright = !sky.fixed_time && sky_light(time % DAY, rain, thunder) > 11.0;

                json!({
                    "timeOfDay": time % DAY,
                    "day": time / DAY,
                    /* The phase is the day count modulo the eight the textures cycle through. */
                    "moonPhase": (time / DAY) % 8,
                    "isDay": bright,
                    "weather": weather,
                    /*
                    null, because a client is never told the game rule. The clock's rate is in the
                    packet, but the other kind of bot cannot read it, and one saying "frozen" where
                    the other says it cannot tell is two answers to one world.
                    */
                    "doDaylightCycle": Value::Null,
                })
            })?;

            Ok(Answer::data("get-world-state", data))
        })
    },
};

struct Sky {
    clock: Option<u32>,
    fixed_time: bool,
    weather: bool,
}

/// The sky light level at a tick of the day: fifteen scaled by the day's timeline, then pulled
/// towards overcast by the rain and the thunder.
fn sky_light(tick: u64, rain: f32, thunder: f32) -> f32 {
    let last = DAYLIGHT.len() - 1;
    let tick = if tick < DAYLIGHT[0].0 { tick + DAY } else { tick };

    let factor = (0..=last)
        .find_map(|index| {
            let (from, start) = DAYLIGHT[index];
            let (to, end) = if index == last { (DAYLIGHT[0].0 + DAY, DAYLIGHT[0].1) } else { DAYLIGHT[index + 1] };
            (tick >= from && tick < to).then(|| start + (end - start) * (tick - from) as f32 / (to - from) as f32)
        })
        .unwrap_or(1.0);

    let blend = |level: f32, weight: f32| level + (OVERCAST - level) * weight;
    blend(blend(15.0 * factor, RAIN_WEIGHT * rain), THUNDER_WEIGHT * thunder)
}
