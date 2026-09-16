use azalea::local_player::LocalGameMode;
use azalea::core::game_type::GameMode;
use azalea::inventory::Menu;
use azalea::protocol::packets::game::{ServerboundSetCommandBlock, ServerboundSignUpdate};
use azalea::{BlockPos, Client};
use serde_json::{Value, json};

use super::args::{boolean, plain, point, position, text};
use super::dialogs::{offered, pick_by};
use super::{Tool, in_world, windows};
use crate::calls::{Answer, Failure, Outcome};
use crate::dialog::{Held, Kind, flatten};
use crate::editors::{CommandEditor, SignEditor, is_sign, sign_face, text_width};
use crate::game::Game;
use crate::hud::block_name;
use crate::menus::Menus;

/// The longest command the editor's box takes.
const COMMAND_LENGTH: usize = 32_500;

const COMMAND_SCREEN: &str = "CommandBlockEditScreen";

/// What the client draws a book from the hand with. One on a lectern is its menu's screen.
const BOOK_SCREEN: &str = "BookViewScreen";

pub const TYPE_TEXT: Tool = Tool {
    name: "type-text",
    run: |bot, args| {
        Box::pin(async move {
            let typed = text(&args, "text")?.to_owned();
            let wanted = match &args["field"] {
                Value::Null => None,
                _ => Some(text(&args, "field")?.to_owned()),
            };
            let replace = boolean(&args, "replace", true)?;

            in_world(&bot, |game| {
                let hud = game.hud.borrow();
                /* The screen on top: a dialog is drawn over an editor, and the editor comes back when it closes. */
                if hud.dialog.is_some() {
                    drop(hud);
                    return type_into_dialog(game, &typed, wanted.as_deref(), replace);
                }
                if hud.sign_editor.is_some() {
                    drop(hud);
                    return type_into_sign(game, &typed, wanted.as_deref(), replace);
                }
                if hud.command_editor.is_some() {
                    drop(hud);
                    return type_into_command_block(game, &typed, wanted.as_deref(), replace);
                }
                Err(nothing_to_type_into(game))
            })?
        })
    },
};

pub const READ_BLOCK_ENTITY: Tool = Tool {
    name: "read-block-entity",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;

            let data = in_world(&bot, |game| {
                let block = block_name(&game.client, at);
                let hud = game.hud.borrow();
                let kept = hud.block_entities.at(at, &block);

                /*
                Both faces of a sign, blanks included. raw stays null as it does on the other kind of
                bot: a client is sent what a block entity shows, not the server's tag, and printing the
                part of it this bot happens to keep would be a third shape for one answer.
                */
                let faces: Vec<Value> = match kept {
                    Some(sign) if is_sign(&sign.kind) => ["front_text", "back_text"]
                        .iter()
                        .map(|face| {
                            let (lines, components) = sign_face(&sign.data, face);
                            json!({"face": face, "lines": lines, "lineComponents": components})
                        })
                        .collect(),
                    _ => Vec::new(),
                };

                json!({
                    "block": plain(&block),
                    "position": point(at),
                    "present": kept.is_some(),
                    "signFaces": faces,
                    "raw": Value::Null,
                })
            })?;
            Ok(Answer::data("read-block-entity", data))
        })
    },
};

/// Right-clicking a command block opens its editor on the client, for a player who may use one:
/// in creative, and an operator. The server answers with the block's command, which fills it in.
pub fn opened_command_block(game: &Game, at: BlockPos) {
    let block = block_name(&game.client, at);
    let command_block = matches!(plain(&block), "command_block" | "chain_command_block" | "repeating_command_block");
    let creative = game.client.get_component::<LocalGameMode>().is_some_and(|mode| mode.current == GameMode::Creative);
    let operator = game.hud.borrow().permission_level >= 2;

    if command_block && creative && operator {
        game.hud.borrow_mut().command_editor = Some(CommandEditor::new(at));
    }
}

/// A press on an editor's button, for press-dialog-button when no dialog is up.
pub fn press(game: &Game, label: &str) -> Outcome {
    let mut hud = game.hud.borrow_mut();

    if let Some(editor) = hud.sign_editor.take() {
        if pick_by(&["Done"], label, |done| done).is_none() {
            let screen = sign_screen(&editor);
            hud.sign_editor = Some(editor);
            return Err(no_such_button(label, screen, ["Done"].into_iter()));
        }
        send_sign(&game.client, &editor);
        return Ok(pressed("Done"));
    }

    let Some(editor) = hud.command_editor.as_mut() else {
        return Err(match screen_on_top(game) {
            Some(screen) => no_such_button(label, screen, std::iter::empty()),
            None => Failure::refused("NO_SCREEN", "no screen is open"),
        });
    };
    let buttons = editor.buttons();
    let Some(index) = pick_by(&buttons, label, String::as_str).and_then(|chosen| buttons.iter().position(|button| std::ptr::eq(button, chosen)))
    else {
        return Err(no_such_button(label, COMMAND_SCREEN, buttons.iter().map(String::as_str)));
    };
    let pressed_label = buttons[index].clone();

    /* Everything but Cancel is off until the block's command has arrived. */
    if !editor.loaded && pressed_label != "Cancel" {
        return Err(Failure::refused("BUTTON_DISABLED", format!("the button \"{pressed_label}\" is disabled")));
    }
    match pressed_label.as_str() {
        "Done" => {
            game.client.write_packet(ServerboundSetCommandBlock {
                pos: editor.at,
                command: editor.command.clone(),
                mode: editor.mode,
                track_output: editor.track_output,
                conditional: editor.conditional,
                automatic: editor.automatic,
            });
            hud.command_editor = None;
        }
        "Cancel" => hud.command_editor = None,
        _ => editor.press(index),
    }
    Ok(pressed(&pressed_label))
}

/// Escape on an editor, for close-window: the title it had and what a player would call it.
pub fn close(game: &Game) -> Option<(String, &'static str)> {
    let mut hud = game.hud.borrow_mut();
    if let Some(editor) = hud.sign_editor.take() {
        /* Closing is what sends a sign, whichever way it is closed. */
        send_sign(&game.client, &editor);
        return Some((editor.title().to_owned(), "sign editor"));
    }
    hud.command_editor.take().map(|_| (String::new(), "command block editor"))
}

/// The screen up when no dialog and no editor is: a book held open over the world, or the one a
/// menu the server opened is drawn by. A refusal names it, as the other kind of bot names the
/// screen it found in place of the one asked for; that screen's own buttons -- a book's "Done", a
/// lectern's "Take Book" -- are not modelled here, and close-window is what closes it.
fn screen_on_top(game: &Game) -> Option<&'static str> {
    if book_open(game) {
        return Some(BOOK_SCREEN);
    }
    windows::menu(&game.client).map(|window| windows::screen(&window.menu))
}

fn book_open(game: &Game) -> bool {
    game.client.get_component::<Menus>().is_some_and(|menus| menus.book.is_some())
}

/// No dialog and no editor to type into. An anvil's name box is a text field the other kind of bot
/// types into and this one does not model, so its refusal is not the one that says the screen has
/// no field: that would be wrong about the screen.
fn nothing_to_type_into(game: &Game) -> Failure {
    if book_open(game) {
        return no_text_field(BOOK_SCREEN);
    }
    match windows::menu(&game.client) {
        Some(window) if matches!(window.menu, Menu::Anvil { .. }) => Failure::refused(
            "UNSUPPORTED_INPUT",
            format!("{} has a name box this bot does not know how to type into", windows::screen(&window.menu)),
        ),
        Some(window) => no_text_field(windows::screen(&window.menu)),
        None => Failure::refused("NO_SCREEN", "no screen is open, so there is nothing to type into"),
    }
}

fn type_into_dialog(game: &Game, typed: &str, wanted: Option<&str>, replace: bool) -> Outcome {
    let mut hud = game.hud.borrow_mut();
    let open = hud.dialog.as_mut().expect("the caller saw a dialog");
    let place = open.screen();
    let fields: Vec<Field> = open
        .inputs()
        .into_iter()
        .filter_map(|input| match input.kind {
            Kind::Text { max_length, multiline } => Some((input.key, flatten(&input.label), max_length, multiline)),
            _ => None,
        })
        .enumerate()
        .map(|(at, (key, label, max_length, multiline))| Field {
            key,
            label: if label.is_empty() { None } else { Some(label) },
            index: at + 1,
            max_length,
            multiline,
            editable: true,
        })
        .collect();

    if fields.is_empty() {
        return Err(no_text_field(place));
    }
    let field = choose(&fields, wanted, place)?;
    let before = match open.held(&field.key) {
        Some(Held::Text(value)) if !replace => value.clone(),
        _ => String::new(),
    };
    let (after, refused) = write(field, &before, typed)?;
    open.hold(&field.key, Held::Text(after.clone()));

    Ok(typed_into(field, place, &after, refused))
}

/// A sign is written a line at a time, from the line asked for down, and the editor is closed
/// afterwards, because closing it is what sends the sign.
fn type_into_sign(game: &Game, typed: &str, wanted: Option<&str>, replace: bool) -> Outcome {
    let mut hud = game.hud.borrow_mut();
    let mut editor = hud.sign_editor.take().expect("the caller saw a sign editor");

    let lines: Vec<&str> = typed.split('\n').collect();
    let first = match wanted {
        None => 0,
        Some(wanted) => match line_of(wanted) {
            Ok(line) => line,
            Err(refused) => {
                hud.sign_editor = Some(editor);
                return Err(refused);
            }
        },
    };
    if first + lines.len() > editor.lines.len() {
        let refused = Failure::refused(
            "SIGN_TOO_SHORT",
            format!("a sign has {} lines, and this writes {} of them from line {}", editor.lines.len(), lines.len(), first + 1),
        );
        hud.sign_editor = Some(editor);
        return Err(refused);
    }

    let width = editor.line_width();
    for (at, line) in lines.iter().enumerate() {
        let written = &mut editor.lines[first + at];
        if replace {
            written.clear();
        }
        /* The editor takes a character only while the line still fits the sign, and refuses nothing. */
        for character in line.chars().filter(|character| allowed(*character)) {
            let longer = format!("{written}{character}");
            if text_width(&longer) <= width {
                *written = longer;
            }
        }
    }

    send_sign(&game.client, &editor);
    let face = if editor.front { "front" } else { "back" };
    let reads: Vec<String> = editor.lines.iter().map(|line| format!("\"{line}\"")).collect();

    Ok(Answer::text(format!(
        "wrote the {face} of the sign, which now reads {}. Closing the editor is what sends it, and it is closed{}",
        reads.join(" / "),
        refused_note(0)
    )))
}

fn type_into_command_block(game: &Game, typed: &str, wanted: Option<&str>, replace: bool) -> Outcome {
    let mut hud = game.hud.borrow_mut();
    let editor = hud.command_editor.as_mut().expect("the caller saw a command block editor");

    /*
    The editor opens before the server has sent the block's command, and when it arrives it is
    written over the field. Typed into before then, the field reads right, Done sends the old
    command, and nothing says the text was ever lost.
    */
    if !editor.loaded {
        return Err(Failure::refused(
            "EDITOR_NOT_LOADED",
            "the command block editor is still waiting for the server to send the block's command, which replaces whatever is typed before it arrives. Wait a few ticks and type again.",
        ));
    }

    let fields = [
        Field { key: "command".into(), label: Some("Console Command".into()), index: 1, max_length: COMMAND_LENGTH, multiline: false, editable: true },
        Field { key: "output".into(), label: Some("Previous Output".into()), index: 2, max_length: COMMAND_LENGTH, multiline: false, editable: false },
    ];
    let field = choose(&fields, wanted, COMMAND_SCREEN)?;
    let before = match (field.editable, replace) {
        (true, true) => String::new(),
        (true, false) => editor.command.clone(),
        (false, _) => String::new(),
    };
    let (after, refused) = write(field, &before, typed)?;
    if field.editable {
        editor.command = after.clone();
    }

    Ok(typed_into(field, COMMAND_SCREEN, &after, refused))
}

fn send_sign(client: &Client, editor: &SignEditor) {
    client.write_packet(ServerboundSignUpdate { pos: editor.at, is_front_text: editor.front, lines: editor.lines.clone() });
}

fn sign_screen(editor: &SignEditor) -> &'static str {
    if editor.hanging { "HangingSignEditScreen" } else { "SignEditScreen" }
}

struct Field {
    key: String,
    label: Option<String>,
    index: usize,
    max_length: usize,
    multiline: bool,
    editable: bool,
}

impl Field {
    fn describe(&self) -> String {
        match &self.label {
            Some(label) => format!("\"{label}\""),
            None => format!("field {}", self.index),
        }
    }
}

fn choose<'a>(fields: &'a [Field], wanted: Option<&str>, place: &str) -> Result<&'a Field, Failure> {
    let described = || fields.iter().map(Field::describe).collect::<Vec<_>>().join(", ");
    let Some(wanted) = wanted else {
        if let [only] = fields {
            return Ok(only);
        }
        return Err(Failure::refused(
            "FIELD_NOT_NAMED",
            format!("{place} has {} text fields and none was asked for. They are {}.", fields.len(), described()),
        ));
    };

    let lowered = wanted.to_lowercase();
    fields
        .iter()
        .find(|field| field.label.as_ref().is_some_and(|label| label.to_lowercase() == lowered) || field.index.to_string() == wanted)
        .or_else(|| fields.iter().find(|field| field.label.as_ref().is_some_and(|label| label.to_lowercase().contains(&lowered))))
        .ok_or_else(|| {
            Failure::refused("NO_SUCH_FIELD", format!("no text field matching \"{wanted}\" on {place}. What it has: {}.", described()))
        })
}

/// Character by character, as the client's text box takes typing: a character a player cannot
/// type, or any character at all in a box that is not editable, is refused and counted; one past
/// the field's length goes nowhere, as it does on screen.
fn write(field: &Field, before: &str, typed: &str) -> Result<(String, usize), Failure> {
    let mut value = before.to_owned();
    let mut refused = 0;

    for character in typed.chars() {
        if character == '\n' {
            if !field.multiline {
                return Err(Failure::refused(
                    "FIELD_IS_ONE_LINE",
                    format!("{} holds one line, and the text asked for more than one", field.describe()),
                ));
            }
        } else if !field.editable || !allowed(character) {
            refused += 1;
            continue;
        }
        if value.encode_utf16().count() + character.len_utf16() <= field.max_length {
            value.push(character);
        }
    }
    if value == before && !typed.is_empty() {
        return Err(Failure::refused(
            "NOTHING_TYPED",
            format!(
                "{} took none of \"{typed}\": either it is read-only, or every character in it is one the client will not send.",
                field.describe()
            ),
        ));
    }
    Ok((value, refused))
}

/// What the client lets a player type into chat and every box built like it.
fn allowed(character: char) -> bool {
    character != '§' && character >= ' ' && character != '\u{7f}'
}

fn line_of(wanted: &str) -> Result<usize, Failure> {
    let digits = wanted.to_lowercase().replace("line", "");
    match digits.trim().parse::<usize>() {
        Ok(line @ 1..=4) if digits.trim().len() == 1 => Ok(line - 1),
        _ => Err(Failure::refused(
            "NO_SUCH_FIELD",
            format!("a sign's fields are its lines, numbered 1 to 4, and \"{wanted}\" is not one of them"),
        )),
    }
}

fn typed_into(field: &Field, place: &str, after: &str, refused: usize) -> Answer {
    Answer::text(format!("typed into {} on {place}, which now reads \"{after}\"{}", field.describe(), refused_note(refused)))
}

fn refused_note(refused: usize) -> String {
    match refused {
        0 => ".".to_owned(),
        1 => ". 1 character did not go in: a field takes what a player could type and no more.".to_owned(),
        many => format!(". {many} characters did not go in: a field takes what a player could type and no more."),
    }
}

fn no_text_field(place: &str) -> Failure {
    Failure::refused(
        "NO_TEXT_FIELD",
        format!("{place} has no text field on it. press-dialog-button presses a button and click-slot moves an item; this is only for somewhere text can be written."),
    )
}

fn no_such_button<'a>(label: &str, screen: &str, buttons: impl Iterator<Item = &'a str>) -> Failure {
    Failure::refused("NO_SUCH_BUTTON", format!("no button matching \"{label}\" on {screen}; it offers {}", offered(buttons)))
}

fn pressed(label: &str) -> Answer {
    Answer::data(format!("pressed \"{label}\""), json!({"label": label, "confirmed": false, "screenAfter": Value::Null}))
}

#[cfg(test)]
mod tests {
    use super::no_such_button;

    /// The sentence a chest or a book gets is the other kind of bot's for a screen with no button
    /// on it, word for word: the end-to-end suite holds both kinds to it.
    #[test]
    fn a_screen_with_no_buttons_is_named_and_offers_none() {
        let refused = no_such_button("Done", "ContainerScreen", std::iter::empty());
        assert_eq!(refused.code, "NO_SUCH_BUTTON");
        assert_eq!(refused.message, "no button matching \"Done\" on ContainerScreen; it offers none");
    }
}
