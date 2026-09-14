use azalea::FormattedText;
use azalea_chat::click_event::ClickEvent;
use serde_json::Value;
use simdnbt::owned::NbtTag;

use super::args::text;
use super::{Tool, in_world, tick};
use crate::calls::{Answer, Failure};
use crate::hud;

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

/// Press something a server wrote in chat: a quest's "[Accept]", a shop's "[Page 2]". Without this
/// the flows a player drives by clicking could only be driven by guessing the command behind them.
///
/// Only the clicks the game makes inside itself are pressed. The rest leave it -- a browser, a
/// file, the clipboard -- and a line from a server does not get to send the host anywhere.
pub const CLICK_CHAT: Tool = Tool {
    name: "click-chat",
    run: |bot, args| {
        Box::pin(async move {
            let wanted = text(&args, "match")?.to_owned();

            let found = {
                let said = bot.said.borrow();
                let offered: Vec<Clickable> = said.iter().flat_map(clickable).collect();
                match pick(&offered, &wanted) {
                    Some(found) => found.clone(),
                    None => {
                        return Err(Failure::refused(
                            "NO_SUCH_CHAT_CLICK",
                            format!("no chat line on screen has a clickable \"{wanted}\" on it. {}", describe(&offered)),
                        ));
                    }
                }
            };

            let mut opened = None;
            match &found.event {
                ClickEvent::RunCommand { command } => {
                    let command = command.strip_prefix('/').unwrap_or(command).to_owned();
                    in_world(&bot, |game| game.client.write_command_packet(&command))?;
                }
                ClickEvent::ShowDialog { dialog } => {
                    /*
                    The client opens it on its own screen and tells the server nothing, so the dialog
                    goes where one the server sent would: the dialog feed, and on screen to be pressed.
                    */
                    let shown = in_world(&bot, |_| match dialog {
                        NbtTag::String(id) => hud::registered_dialog(&bot, &id.to_str()),
                        inline => serde_json::to_value(inline).ok(),
                    })?;
                    let Some(shown) = shown else {
                        return Err(Failure::refused(
                            "NO_SUCH_DIALOG",
                            format!("\"{}\" shows a dialog the server never declared, so there is nothing to open.", found.text),
                        ));
                    };
                    opened = Some(screen(&shown));
                    hud::show_dialog(&bot, Some(shown));
                }
                other => {
                    return Err(Failure::refused(
                        "CLICK_NOT_IN_GAME",
                        format!(
                            "\"{}\" is a {} click, and only run_command and show_dialog are pressed from here. The rest leave the game -- a browser, a file, the clipboard -- which is not this bot's to do.",
                            found.text,
                            action(other)
                        ),
                    ));
                }
            }

            /* What the click turns into arrives a tick or two later, as it does for a player. */
            for _ in 0..SETTLE_TICKS {
                tick(&bot).await;
            }
            let opened = opened.map(|screen| format!(", and {screen} opened")).unwrap_or_default();
            Ok(Answer::text(format!("clicked {}{opened}.", found.describe())))
        })
    },
};

const SETTLE_TICKS: usize = 3;

/// Enough of what is clickable to tell a misspelling from a menu that never appeared.
const MAX_LISTED: usize = 8;

/// One clickable run of a chat line: what it reads as, and what pressing it does.
#[derive(Clone)]
struct Clickable {
    text: String,
    event: ClickEvent,
}

impl Clickable {
    fn describe(&self) -> String {
        format!("\"{}\" ({})", self.text, action(&self.event))
    }
}

fn action(event: &ClickEvent) -> &'static str {
    match event {
        ClickEvent::OpenUrl { .. } => "open_url",
        ClickEvent::OpenFile { .. } => "open_file",
        ClickEvent::RunCommand { .. } => "run_command",
        ClickEvent::SuggestCommand { .. } => "suggest_command",
        ClickEvent::ShowDialog { .. } => "show_dialog",
        ClickEvent::ChangePage { .. } => "change_page",
        ClickEvent::CopyToClipboard { .. } => "copy_to_clipboard",
        ClickEvent::Custom { .. } => "custom",
    }
}

/// Every run of a line that carries a click event, with the text that run reads as. A run and not
/// the whole line: "Quest: chop wood [Accept] [Decline]" is one line with two of them.
fn clickable(line: &FormattedText) -> Vec<Clickable> {
    let base = line.get_base();
    if let Some(event) = &base.style.click_event {
        return vec![Clickable { text: line.to_string(), event: event.clone() }];
    }
    base.siblings.iter().flat_map(clickable).collect()
}

/// The client's own name for the screen a dialog is drawn on, which is what the other kind of bot
/// says opened.
fn screen(dialog: &Value) -> &'static str {
    match dialog.get("type").and_then(Value::as_str).map(|kind| kind.strip_prefix("minecraft:").unwrap_or(kind)) {
        Some("multi_action") => "MultiButtonDialogScreen",
        Some("dialog_list") => "DialogListDialogScreen",
        Some("server_links") => "ServerLinksDialogScreen",
        _ => "SimpleDialogScreen",
    }
}

fn pick<'a>(offered: &'a [Clickable], wanted: &str) -> Option<&'a Clickable> {
    let lowered = wanted.to_lowercase();
    offered
        .iter()
        .find(|one| one.text.to_lowercase() == lowered)
        .or_else(|| offered.iter().find(|one| one.text.to_lowercase().contains(&lowered)))
}

fn describe(offered: &[Clickable]) -> String {
    if offered.is_empty() {
        return "Nothing said in chat since this bot joined is clickable at all.".to_owned();
    }
    let listed: Vec<String> = offered.iter().take(MAX_LISTED).map(Clickable::describe).collect();
    let more = if offered.len() > MAX_LISTED { format!(", and {} more", offered.len() - MAX_LISTED) } else { String::new() };
    format!("What is: {}{more}.", listed.join(", "))
}
