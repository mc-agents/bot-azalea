use super::{Tool, tick};
use crate::calls::{Answer, Failure};

pub const WAIT_TICKS: Tool = Tool {
    name: "wait-ticks",
    run: |bot, args| {
        Box::pin(async move {
            let ticks = args["ticks"].as_u64().filter(|ticks| *ticks >= 1).ok_or_else(|| Failure::bad_args("ticks must be at least 1"))?;

            /*
            The server's ticks as the client runs them, which is what "wait for the plugin to react"
            means. A proxy moving the bot between servers stops them until it arrives, and the wait
            spans the move; a connection that ends stops them for good, and the wait says the bot
            left rather than sitting out the call's deadline.
            */
            let target = *bot.ticks.borrow() + ticks;
            while *bot.ticks.borrow() < target {
                if bot.game.borrow().is_none() {
                    return Err(Failure::not_in_game());
                }
                tick(&bot).await;
            }
            Ok(Answer::text(format!("Waited {ticks} tick(s).")))
        })
    },
};

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use serde_json::json;

    use super::WAIT_TICKS;
    use crate::bot::Bot;
    use crate::config::Config;

    /// With no connection there are no ticks to count, and the wait said so at once where it once
    /// slept a wall-clock tick for each one asked for and answered that it had waited.
    #[tokio::test]
    async fn without_a_connection_the_wait_says_the_bot_is_not_in_a_world() {
        let bot = Rc::new(Bot::new(Config::from_env()));

        let outcome = tokio::time::timeout(Duration::from_millis(500), (WAIT_TICKS.run)(bot, json!({"ticks": 400}))).await;

        match outcome {
            Ok(Err(refused)) => assert_eq!(refused.code, "NOT_IN_GAME"),
            Ok(Ok(answer)) => panic!("waited: {}", answer.text),
            Err(_) => panic!("still waiting after 500ms"),
        }
    }
}
