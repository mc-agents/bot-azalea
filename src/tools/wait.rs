use std::time::Duration;

use super::Tool;
use crate::calls::{Answer, Failure};

/// How long a tick is when there is no server counting them.
const TICK: Duration = Duration::from_millis(50);

pub const WAIT_TICKS: Tool = Tool {
    name: "wait-ticks",
    run: |bot, args| {
        Box::pin(async move {
            let ticks = args["ticks"].as_u64().filter(|ticks| *ticks >= 1).ok_or_else(|| Failure::bad_args("ticks must be at least 1"))?;

            /*
            In a world, the server's ticks as the client runs them, which is what "wait for the plugin
            to react" means. Out of one there is nothing to count, and a wall-clock tick keeps the
            call honest about how long it waited.
            */
            let in_world = bot.game.borrow().as_ref().is_some_and(|game| game.spawned());
            if in_world {
                let mut counter = bot.ticks.subscribe();
                let target = *counter.borrow_and_update() + ticks;
                while *counter.borrow_and_update() < target {
                    if counter.changed().await.is_err() {
                        break;
                    }
                }
            } else {
                tokio::time::sleep(TICK * ticks as u32).await;
            }
            Ok(Answer::text(format!("Waited {ticks} tick(s).")))
        })
    },
};
