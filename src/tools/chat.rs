use super::{Tool, in_world};
use crate::calls::{Answer, Failure};

pub const SEND_CHAT: Tool = Tool {
    name: "send-chat",
    run: |bot, args| {
        Box::pin(async move {
            let message = args["message"].as_str().ok_or_else(|| Failure::bad_args("expected a string for message"))?;

            /*
            A slash is refused rather than sent. run-command exists because the server's answer to a
            command is chat somebody has to collect, and a command sent through here is sent with
            nobody watching for the reply.
            */
            if message.starts_with('/') {
                return Err(Failure::refused(
                    "NOT_A_COMMAND",
                    "send-chat is for plain chat. Use run-command to send a slash command.",
                ));
            }

            let username = in_world(&bot, |game| {
                game.client.chat(message);
                game.username.clone()
            })?;
            Ok(Answer::text(format!("Sent as {username}: {message}")))
        })
    },
};
