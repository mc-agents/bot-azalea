use std::time::Duration;

use serde_json::json;

use super::{Tool, in_world};
use crate::calls::{Answer, Failure};
use crate::worldedit;

/// What WorldEdit has selected, asked of the server rather than read out of chat.
///
/// An announcement is sent and the description that comes back is the answer. That is a round trip
/// and not a look at kept state, because the point of the tool is to know the selection as it
/// stands now: a `//pos1` the server has not finished handling yet has changed nothing to describe,
/// and an answer assembled from the last description would report the corner before it.
///
/// A server that never answers is a server without WorldEdit, or one whose WorldEdit does not send
/// CUI, and that is `supported: false` rather than a refusal -- the caller's next move is to fall
/// back to reading chat, which needs to be told the difference.
pub const READ_SELECTION: Tool = Tool {
    name: "read-selection",
    run: |bot, args| {
        Box::pin(async move {
            let timeout = Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(1_000));
            let asked = in_world(&bot, |game| {
                let described = game.hud.borrow().selection.described();

                worldedit::ask(&game.client);
                described
            })?;

            let mut ticks = bot.ticks.subscribe();
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);

            /*
            One description is several messages -- the shape, then a corner each -- and they are not
            promised to land in one tick. So the answer waits for a tick that adds nothing to the
            one before it, and what it reads then is a whole description.
            */
            let mut quiet = asked;
            let supported = loop {
                tokio::select! {
                    changed = ticks.changed() => if changed.is_err() {
                        return Err(Failure::not_in_game());
                    },
                    _ = &mut deadline => break false,
                }

                let described = in_world(&bot, |game| game.hud.borrow().selection.described())?;

                if described > asked {
                    if described == quiet {
                        break true;
                    }
                    quiet = described;
                }
            };

            /*
            Nothing described is nothing known. Answering with what the last server said, or with
            what this one said before the bot asked, would read as "nothing is selected" -- which is
            a different thing from "this server does not say".
            */
            if !supported {
                return Ok(Answer::data(
                    "read-selection",
                    json!({"supported": false, "shape": null, "points": [], "volume": null}),
                ));
            }

            let selection = in_world(&bot, |game| {
                let hud = game.hud.borrow();
                let selection = &hud.selection;
                let points: Vec<_> = selection
                    .corners()
                    .map(|(index, at)| json!({"index": index, "x": at.x, "y": at.y, "z": at.z}))
                    .collect();

                json!({
                    "supported": true,
                    "shape": selection.shape(),
                    "points": points,
                    "volume": selection.volume(),
                })
            })?;

            Ok(Answer::data("read-selection", selection))
        })
    },
};
