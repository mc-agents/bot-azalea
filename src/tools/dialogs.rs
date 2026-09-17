use azalea::buf::{AzBuf, AzBufVar};
use azalea::protocol::packets::game::c_commands::{BrigadierParser, BrigadierString, ClientboundCommands, NodeType};
use azalea::protocol::packets::game::s_custom_click_action::ServerboundCustomClickAction;
use azalea::protocol::packets::{Packet, ProtocolPacket};
use azalea::{Client, Identifier};
use serde_json::{Value, json};
use simdnbt::owned::{Nbt, NbtTag};

use super::args::text;
use super::{Tool, in_world};
use crate::calls::{Answer, Failure};
use crate::dialog::{Action, Button, Held, Kind, Open, flatten, slider_text};
use crate::feeds;

/// The most tokens one argument is tried against. A position or a rotation is several words; a
/// longer run is not one argument any command a dialog runs takes.
const WIDEST_ARGUMENT: usize = 4;

pub const PRESS_DIALOG_BUTTON: Tool = Tool {
    name: "press-dialog-button",
    run: |bot, args| {
        Box::pin(async move {
            let label = text(&args, "label")?.to_owned();

            in_world(&bot, |game| {
                let mut hud = game.hud.borrow_mut();
                let Some(open) = hud.dialog.as_ref() else {
                    drop(hud);
                    return super::editors::press(game, &label);
                };
                let buttons = open.buttons();
                let button = pick(&buttons, &label).ok_or_else(|| {
                    Failure::refused(
                        "NO_SUCH_BUTTON",
                        format!(
                            "no button matching \"{label}\" on {}; it offers {}",
                            open.screen(),
                            offered(buttons.iter().map(|button| button.label.as_str()))
                        ),
                    )
                })?;

                let pressed = button.label.clone();
                let ran = button
                    .action
                    .as_ref()
                    .map(|action| run(&game.client, hud.commands.as_ref(), open, action, &pressed));
                let stays = open.stays_open_after();
                let screen = open.screen();

                /* A command the client will not run takes the dialog down with it, as the client does. */
                if !stays || matches!(ran, Some(Err(_))) {
                    hud.dialog = None;
                }
                let confirmed = ran.transpose()?.unwrap_or(false);

                let mut data = json!({"label": pressed, "confirmed": confirmed, "screenAfter": if stays { json!(screen) } else { Value::Null }});
                let text = if confirmed {
                    data["confirmedWith"] = json!(RUN_COMMAND);
                    data["confirmTitle"] = json!(CONFIRM_TITLE);
                    format!("pressed \"{pressed}\", then \"{RUN_COMMAND}\" on {CONFIRM_TITLE}")
                } else {
                    format!("pressed \"{pressed}\"")
                };
                Ok(Answer::data(text, data))
            })?
        })
    },
};

pub const SET_DIALOG_INPUT: Tool = Tool {
    name: "set-dialog-input",
    run: |bot, args| {
        Box::pin(async move {
            let key = text(&args, "key")?.to_owned();
            let wanted = args["value"].clone();

            let (data, feed) = in_world(&bot, |game| {
                let mut hud = game.hud.borrow_mut();
                let Some(open) = hud.dialog.as_mut() else {
                    return Err(Failure::refused(
                        "NO_DIALOG",
                        "no dialog is open, so there is no input to set",
                    ));
                };
                let inputs = open.inputs();
                let input = inputs.iter().find(|input| input.key == key).ok_or_else(|| {
                    Failure::refused(
                        "NO_SUCH_INPUT",
                        if inputs.is_empty() {
                            "the dialog has no inputs".to_owned()
                        } else {
                            format!(
                                "the dialog has no input \"{key}\"; its inputs are {}",
                                inputs
                                    .iter()
                                    .map(|input| input.key.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        },
                    )
                })?;
                let previous = open.held(&key).cloned();

                let (kind, now, display, requested) = match &input.kind {
                    Kind::Checkbox { .. } => {
                        let Value::Bool(ticked) = wanted else {
                            return Err(wrong_type(&key, "a checkbox", "true or false", &wanted));
                        };
                        ("boolean", Held::Checkbox(ticked), Value::Null, Value::Null)
                    }
                    Kind::Choice { entries } => {
                        let Value::String(asked) = &wanted else {
                            return Err(wrong_type(
                                &key,
                                "a cycle",
                                "the id or the text of one of its options",
                                &wanted,
                            ));
                        };
                        let chosen = entries
                            .iter()
                            .find(|entry| entry.id == *asked)
                            .or_else(|| {
                                entries
                                    .iter()
                                    .find(|entry| entry.display.to_lowercase() == asked.to_lowercase())
                            })
                            .ok_or_else(|| {
                                Failure::refused(
                                    "NO_SUCH_OPTION",
                                    format!(
                                        "\"{key}\" has no option \"{asked}\"; it offers {}",
                                        entries
                                            .iter()
                                            .map(|entry| format!("{} (\"{}\")", entry.id, entry.display))
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ),
                                )
                            })?;
                        (
                            "single_option",
                            Held::Choice(chosen.id.clone()),
                            json!(chosen.display),
                            Value::Null,
                        )
                    }
                    Kind::Slider(range) => {
                        let Some(asked) = wanted.as_f64().map(|number| number as f32) else {
                            return Err(wrong_type(&key, "a slider", "a number", &wanted));
                        };
                        let (low, high) = (range.start.min(range.end), range.start.max(range.end));
                        if asked < low || asked > high {
                            return Err(Failure::refused(
                                "OUT_OF_RANGE",
                                format!(
                                    "\"{key}\" goes from {} to {}, and {} is outside it",
                                    slider_text(range.start),
                                    slider_text(range.end),
                                    slider_text(asked)
                                ),
                            ));
                        }
                        /* Where a drag would leave the handle, and then the number the slider sends from there. */
                        let now = range.scaled(range.to_slider(asked));
                        (
                            "number_range",
                            Held::Slider(now),
                            Value::Null,
                            if now == asked { Value::Null } else { json!(asked) },
                        )
                    }
                    Kind::Text { .. } => {
                        return Err(Failure::refused(
                            "TEXT_INPUT",
                            format!("\"{key}\" is a text field; type into it with type-text"),
                        ));
                    }
                    Kind::Unknown => {
                        return Err(Failure::refused(
                            "UNSUPPORTED_INPUT",
                            format!("\"{key}\" is an input this bot does not know how to set"),
                        ));
                    }
                };

                let data = json!({
                    "key": key,
                    "type": kind,
                    "label": flatten(&input.label),
                    "labelComponent": input.label,
                    "value": held(&now),
                    "previous": previous.as_ref().map(held).unwrap_or(Value::Null),
                    "display": display,
                    "requested": requested,
                });
                open.hold(&key, now);

                /* The dialog again with what its inputs hold now, so a read of the feed tells what a button will send. */
                let mut feed = open.definition.clone();
                feed["values"] = open.values();
                Ok((data, feed))
            })??;

            feeds::dialog(&bot, feed);
            Ok(Answer::data(format!("set \"{key}\""), data))
        })
    },
};

/// Run what a dialog button is bound to. True when the client would have asked to confirm the
/// command first -- and been told yes -- which the reply says the way the other kind of bot does.
pub fn run(
    client: &Client,
    commands: Option<&ClientboundCommands>,
    open: &Open,
    action: &Value,
    label: &str,
) -> Result<bool, Failure> {
    match open.action(action) {
        Action::Command(command) => {
            let command = command.strip_prefix('/').unwrap_or(&command).to_owned();
            let check = verify(commands, &command);
            if check == Check::SignatureRequired {
                return Err(Failure::refused(
                    "COMMAND_NOT_RUN",
                    format!(
                        "\"{label}\" asks the client to run a command and it will not: all it offers is \"{SUGGEST_COMMAND}\". A command that sends chat as the player can only be run from the chat screen, so an action built on one does nothing when pressed. This is the server's to fix, not the bot's."
                    ),
                ));
            }
            client.write_command_packet(&command);
            Ok(check != Check::NoIssues)
        }
        Action::Custom { id, payload } => {
            custom_click(client, &id, payload.as_ref())?;
            Ok(false)
        }
        Action::Local => Ok(false),
    }
}

const CONFIRM_TITLE: &str = "Confirm Command Execution";
const RUN_COMMAND: &str = "Run Command";
const SUGGEST_COMMAND: &str = "Copy to Chat Screen";

/// Upstream: `ServerboundCustomClickAction` in azalea-protocol 0.16.0 (`s_custom_click_action.rs`)
/// writes its payload as a bare NBT tag. 26.1 reads an optional tag behind a length prefix, and a
/// server that reads the packet azalea writes drops the connection. Once azalea matches it, this is
/// `client.write_packet(ServerboundCustomClickAction { .. })`.
fn custom_click(client: &Client, id: &str, payload: Option<&NbtTag>) -> Result<(), Failure> {
    let id = Identifier::new(id);
    let mut tag = Vec::new();
    match payload {
        Some(payload) => payload.write(&mut tag),
        None => Nbt::None.write(&mut tag),
    }

    let mut packet = Vec::new();
    let _ = ServerboundCustomClickAction {
        id: id.clone(),
        payload: Nbt::None,
    }
    .into_variant()
    .id()
    .azalea_write_var(&mut packet);
    let _ = id.azalea_write(&mut packet);
    let _ = (tag.len() as u32).azalea_write_var(&mut packet);
    packet.extend_from_slice(&tag);

    client
        .with_raw_connection_mut(|mut connection| {
            connection.net_conn().map(|network| network.write_raw(&packet).is_ok())
        })
        .filter(|written| *written)
        .map(|_| ())
        .ok_or_else(Failure::not_in_game)
}

/// What the client makes of a command before it sends one, as `ClientPacketListener.verifyCommand`
/// decides it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Check {
    NoIssues,
    /// Unknown to the tree, or not a whole command: the client asks, and yes sends it anyway.
    ParseErrors,
    /// A node the tree marks restricted: the client asks, and yes sends it.
    PermissionsRequired,
    /// An argument the player would sign, which the client will only send from the chat screen.
    SignatureRequired,
}

/// Parsed against the command tree the server sent, the way brigadier walks it: literals before
/// arguments, a greedy argument taking the rest, a redirect carrying on from where it points.
fn verify(tree: Option<&ClientboundCommands>, command: &str) -> Check {
    let Some(tree) = tree else { return Check::ParseErrors };
    let tokens: Vec<&str> = command.split(' ').filter(|token| !token.is_empty()).collect();

    match walk(tree, tree.root_index as usize, &tokens, false, false) {
        None => Check::ParseErrors,
        Some((true, _)) => Check::SignatureRequired,
        Some((false, true)) => Check::PermissionsRequired,
        Some((false, false)) => Check::NoIssues,
    }
}

fn walk(
    tree: &ClientboundCommands,
    at: usize,
    tokens: &[&str],
    signed: bool,
    restricted: bool,
) -> Option<(bool, bool)> {
    let node = tree.entries.get(at)?;
    if tokens.is_empty() {
        return node.is_executable.then_some((signed, restricted));
    }
    let children = match node.redirect_node {
        Some(target) => &tree.entries.get(target as usize)?.children,
        None => &node.children,
    };

    for &child in children {
        let next = tree.entries.get(child as usize)?;
        if let NodeType::Literal { name } = &next.node_type
            && name == tokens[0]
            && let Some(found) = walk(
                tree,
                child as usize,
                &tokens[1..],
                signed,
                restricted || next.is_restricted,
            )
        {
            return Some(found);
        }
    }
    for &child in children {
        let next = tree.entries.get(child as usize)?;
        let NodeType::Argument { parser, .. } = &next.node_type else {
            continue;
        };
        let restricted = restricted || next.is_restricted;

        if matches!(
            parser,
            BrigadierParser::Message | BrigadierParser::String(BrigadierString::GreedyPhrase)
        ) {
            if next.is_executable {
                return Some((signed || matches!(parser, BrigadierParser::Message), restricted));
            }
            continue;
        }
        for taken in 1..=tokens.len().min(WIDEST_ARGUMENT) {
            if let Some(found) = walk(tree, child as usize, &tokens[taken..], signed, restricted) {
                return Some(found);
            }
        }
    }
    None
}

fn pick<'a>(buttons: &'a [Button], label: &str) -> Option<&'a Button> {
    pick_by(buttons, label, |button| &button.label)
}

/// By the label a player reads: all of it first, any part of it after, both ignoring case.
pub fn pick_by<'a, T>(items: &'a [T], label: &str, shown: impl Fn(&T) -> &str) -> Option<&'a T> {
    let lowered = label.to_lowercase();
    items
        .iter()
        .find(|item| shown(item).to_lowercase() == lowered)
        .or_else(|| items.iter().find(|item| shown(item).to_lowercase().contains(&lowered)))
}

pub fn offered<'a>(labels: impl Iterator<Item = &'a str>) -> String {
    let quoted: Vec<String> = labels.map(|label| format!("\"{label}\"")).collect();
    if quoted.is_empty() {
        "none".to_owned()
    } else {
        quoted.join(", ")
    }
}

fn held(held: &Held) -> Value {
    match held {
        Held::Text(text) | Held::Choice(text) => json!(text),
        Held::Checkbox(ticked) => json!(ticked),
        Held::Slider(number) => json!(number),
    }
}

fn wrong_type(key: &str, what: &str, takes: &str, got: &Value) -> Failure {
    Failure::refused(
        "WRONG_VALUE_TYPE",
        format!("\"{key}\" is {what} and takes {takes}, not {got}"),
    )
}
