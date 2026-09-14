use azalea::FormattedText;
use azalea::protocol::packets::game::c_boss_event::{BossBarColor, BossBarOverlay};
use serde_json::json;

use super::{Tool, game_mode, in_world};
use crate::calls::Answer;
use crate::hud::Slot;
use crate::text;

/// What a server is drawing in a display slot, usually the sidebar.
pub const READ_SCOREBOARD: Tool = Tool {
    name: "read-scoreboard",
    run: |bot, args| {
        Box::pin(async move {
            let wanted = args["slot"].as_str().unwrap_or("sidebar").to_owned();
            let slot = match wanted.as_str() {
                "list" => Slot::List,
                "belowName" => Slot::BelowName,
                _ => Slot::Sidebar,
            };

            let board = in_world(&bot, |game| {
                let hud = game.hud.borrow();

                /* Nothing displayed is a state, not a failure, so the server says the words. */
                hud.board(slot).map(|(objective, entries)| {
                    let entries: Vec<_> = entries
                        .into_iter()
                        .map(|(owner, score)| {
                            /*
                            The name the server gave the entry when it gave one, which is what the
                            sidebar draws. The owner is the key it scores against -- a uuid or an
                            internal handle on the servers that use one.
                            */
                            let name = score.display.clone().unwrap_or_else(|| FormattedText::from(owner));
                            json!({"name": name.to_string(), "nameComponent": text::component(&name), "score": score.value})
                        })
                        .collect();

                    json!({
                        "title": objective.title.to_string(),
                        "titleComponent": text::component(&objective.title),
                        "entries": entries,
                    })
                })
            })?;

            Ok(Answer::data("read-scoreboard", json!({"slot": wanted, "board": board})))
        })
    },
};

/// Boss bars the server has put on the HUD, which servers use as a progress display.
pub const READ_BOSS_BARS: Tool = Tool {
    name: "read-boss-bars",
    run: |bot, _args| {
        Box::pin(async move {
            let bars = in_world(&bot, |game| {
                game.hud
                    .borrow()
                    .bars
                    .iter()
                    .map(|bar| {
                        json!({
                            "title": bar.name.to_string(),
                            "progress": decimal(bar.progress),
                            "color": color(bar.color),
                            "dividers": dividers(bar.overlay),
                            /* Stacked labels, the same as an action bar: joined into one string they run together. */
                            "segments": text::segments(&bar.name),
                            "component": text::component(&bar.name),
                        })
                    })
                    .collect::<Vec<_>>()
            })?;

            Ok(Answer::data("read-boss-bars", json!({"bars": bars})))
        })
    },
};

/// Who else is online, from the tab list the server keeps up to date.
pub const READ_PLAYER_LIST: Tool = Tool {
    name: "read-player-list",
    run: |bot, _args| {
        Box::pin(async move {
            let players = in_world(&bot, |game| {
                let me = game.client.username();
                let hud = game.hud.borrow();

                let mut players: Vec<_> = game
                    .client
                    .tab_list()
                    .into_values()
                    .filter(|info| hud.listed(info.uuid.as_u128()))
                    .collect();
                players.sort_by(|a, b| a.profile.name.cmp(&b.profile.name));

                players
                    .into_iter()
                    .map(|info| {
                        /*
                        The username is the identity every other tool takes; what the tab list draws
                        is where a server puts a rank, in the pack's own font as often as not.
                        */
                        let drawn = info.display_name.as_deref();
                        json!({
                            "name": info.profile.name,
                            "gameMode": game_mode(Some(info.gamemode)),
                            "ping": info.latency,
                            "self": info.profile.name == me,
                            "displayName": drawn.map(ToString::to_string),
                            "displayNameComponent": drawn.map(text::component),
                        })
                    })
                    .collect::<Vec<_>>()
            })?;

            Ok(Answer::data("read-player-list", json!({"players": players})))
        })
    },
};

/// A float as the shortest decimal that reads back as it, the way the other kind of bot writes one:
/// widened as it is, 0.73 goes on the wire as 0.7300000190734863.
fn decimal(value: f32) -> f64 {
    value.to_string().parse().unwrap_or_else(|_| f64::from(value))
}

fn color(color: BossBarColor) -> &'static str {
    match color {
        BossBarColor::Pink => "pink",
        BossBarColor::Blue => "blue",
        BossBarColor::Red => "red",
        BossBarColor::Green => "green",
        BossBarColor::Yellow => "yellow",
        BossBarColor::Purple => "purple",
        BossBarColor::White => "white",
    }
}

/// The notch count a style draws, which is what a reader counts. Zero for a plain bar.
fn dividers(overlay: BossBarOverlay) -> u32 {
    match overlay {
        BossBarOverlay::Progress => 0,
        BossBarOverlay::Notched6 => 6,
        BossBarOverlay::Notched10 => 10,
        BossBarOverlay::Notched12 => 12,
        BossBarOverlay::Notched20 => 20,
    }
}
