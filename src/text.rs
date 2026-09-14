use std::cell::RefCell;

use azalea::FormattedText;
use serde_json::{Value, json};

/// The component as Minecraft's own JSON, as far as azalea kept it, for mcp-server to flatten.
pub fn component(component: &FormattedText) -> Value {
    serde_json::to_value(component).unwrap_or_default()
}

/// The pieces a component is drawn from, with the glyphs taken out.
///
/// A server draws a HUD by stacking a bar glyph, a spacer and a label. The glyphs are private use
/// area codepoints that mean nothing as text, and a spacer with them removed holds whitespace and
/// nothing else: a position on the screen rather than something to read, so it is not a segment.
/// A piece that was only ever a space is text, though: a chat line built as "Cleared 0", a space and
/// "[Track]" read "Cleared 0[Track]" while the screen showed the gap.
///
/// What is left is not trimmed. "Mana " and "Mana" are different pieces, and a server that writes
/// a label and a number as two components puts the space in one of them.
pub fn segments(component: &FormattedText) -> Value {
    let style = RefCell::new((None, None));
    let pieces = RefCell::new(Vec::new());

    /*
    azalea walks the tree with the style each piece inherits and hands the style and the text to
    two separate closures, one straight after the other; the first remembers what the second needs.
    The parent style starts empty: azalea's own default is white, and every piece would claim it.
    */
    component.to_custom_format(
        |_, inherited| {
            *style.borrow_mut() = (font(inherited.font.as_deref()), inherited.color.as_ref().map(ToString::to_string));
            (String::new(), String::new())
        },
        |text| {
            let readable: String = text.chars().filter(|c| !glyph(*c)).collect();
            let spacer = blank(&readable) && readable.chars().count() != text.chars().count();
            if !readable.is_empty() && !spacer {
                let (font, color) = style.borrow().clone();
                let mut segment = json!({"text": readable});
                if let Some(font) = font {
                    segment["font"] = json!(font);
                }
                if let Some(color) = color {
                    segment["color"] = json!(color);
                }
                pieces.borrow_mut().push(segment);
            }
            String::new()
        },
        |_| String::new(),
        &Default::default(),
    );

    Value::Array(pieces.into_inner())
}

/// The text as it reads with the glyphs taken out and nothing between the pieces: what a pattern
/// written against a feed line is matched with inside the bot, where the font labels mcp-server puts
/// in front of each piece do not exist.
pub fn readable(component: &FormattedText) -> String {
    component.to_string().chars().filter(|c| !glyph(*c)).collect()
}

/// The font by its full name, and none for the default one, which is what names no label.
fn font(font: Option<&str>) -> Option<String> {
    let font = font?;
    let named = if font.contains(':') { font.to_owned() } else { format!("minecraft:{font}") };
    (named != "minecraft:default").then_some(named)
}

/// The private use areas, where a resource pack puts the glyphs it draws a HUD out of.
fn glyph(c: char) -> bool {
    matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{10FFFF}')
}

/// Blank the way Java's isBlank means it, which is what the other kind of bot drops by. Rust's
/// whitespace also counts the no-break spaces, which a HUD uses precisely because they are not.
fn blank(text: &str) -> bool {
    text.chars().all(|c| match c {
        '\u{00A0}' | '\u{2007}' | '\u{202F}' => false,
        '\u{1C}'..='\u{1F}' => true,
        c => c.is_whitespace(),
    })
}
