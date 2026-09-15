use std::cell::RefCell;

use azalea::FormattedText;
use regex::Regex;
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
    let pieces = leaves(component)
        .into_iter()
        .filter_map(|leaf| {
            let readable: String = leaf.text.chars().filter(|c| !glyph(*c)).collect();
            if readable.is_empty() || spacer(&readable, &leaf.text) {
                return None;
            }
            let mut segment = json!({"text": readable});
            if let Some(font) = font(leaf.font.as_deref()) {
                segment["font"] = json!(font);
            }
            if let Some(color) = leaf.color {
                segment["color"] = json!(color);
            }
            Some(segment)
        })
        .collect();

    Value::Array(pieces)
}

/// A line in the three wordings a pattern can be written against.
///
/// A caller writes a pattern from what a tool showed, and a tool shows a line with the glyphs and
/// colour codes taken out and a label in front of each piece drawn in a font of its own; a pattern
/// written before there were labels matched the plain text, and one pasted from the wire holds the
/// glyphs themselves. A pattern that matches any of the three matches the line.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// The text as the component reads, glyphs and all.
    pub raw: String,
    /// With the glyphs and the colour codes taken out.
    pub readable: String,
    /// As mcp-server shows it.
    pub shown: String,
}

impl Line {
    pub fn of(component: &FormattedText) -> Line {
        let raw = component.to_string();
        Line { readable: readable(&raw), shown: shown(component, &raw), raw }
    }

    /// A line that reads one way only: a sound's or a particle's id.
    pub fn plain(id: String) -> Line {
        Line { raw: id.clone(), readable: id.clone(), shown: id }
    }

    pub fn matches(&self, pattern: &Regex) -> bool {
        pattern.is_match(&self.shown) || pattern.is_match(&self.readable) || pattern.is_match(&self.raw)
    }
}

/// The text with the glyphs taken out, and then the legacy colour codes: what a line reads as once
/// nothing that only positions or colours it is left.
pub fn readable(text: &str) -> String {
    let mut readable = String::with_capacity(text.len());
    let mut chars = text.chars().filter(|c| !glyph(*c)).peekable();
    while let Some(c) = chars.next() {
        if c == '§' && chars.peek().is_some_and(|code| colour_code(*code)) {
            chars.next();
            continue;
        }
        readable.push(c);
    }
    readable
}

/// The line as mcp-server shows it, which is its Flatten and Piece mirrored: each leaf with the
/// glyphs and colour codes taken out and dropped when nothing readable is left; then, when no piece
/// has a font, the pieces run together as drawn, and when one does, each piece that is not blank
/// goes as "[font] text" -- the font without its namespace -- joined with " | ". A component with
/// nothing readable in it shows as the text it reads.
fn shown(component: &FormattedText, raw: &str) -> String {
    let pieces: Vec<(String, Option<String>)> = leaves(component)
        .into_iter()
        .filter_map(|leaf| {
            let text = readable(&leaf.text);
            (!text.is_empty() && !spacer(&text, &leaf.text)).then_some((text, leaf.font))
        })
        .collect();

    if pieces.is_empty() {
        return raw.to_owned();
    }
    if pieces.iter().all(|(_, font)| font.is_none()) {
        return pieces.into_iter().map(|(text, _)| text).collect();
    }
    pieces
        .iter()
        .filter(|(text, _)| !blank(text))
        .map(|(text, font)| match font {
            Some(font) => format!("[{}] {text}", font.split_once(':').map_or(font.as_str(), |(_, name)| name)),
            None => text.clone(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// One leaf of a component, with the font and colour it is drawn in: its own, or inherited.
struct Leaf {
    text: String,
    font: Option<String>,
    color: Option<String>,
}

/// The leaves of a component in the order they are drawn, a translate key resolved to its text the
/// way the client resolves it.
fn leaves(component: &FormattedText) -> Vec<Leaf> {
    let style = RefCell::new((None, None));
    let leaves = RefCell::new(Vec::new());

    /*
    azalea walks the tree with the style each piece inherits and hands the style and the text to
    two separate closures, one straight after the other; the first remembers what the second needs.
    The parent style starts empty: azalea's own default is white, and every piece would claim it.
    */
    component.to_custom_format(
        |_, inherited| {
            *style.borrow_mut() = (inherited.font.clone(), inherited.color.as_ref().map(ToString::to_string));
            (String::new(), String::new())
        },
        |text| {
            let (font, color) = style.borrow().clone();
            leaves.borrow_mut().push(Leaf { text: text.to_owned(), font, color });
            String::new()
        },
        |_| String::new(),
        &Default::default(),
    );

    leaves.into_inner()
}

/// A leaf that is blank once its glyphs are gone, and was not blank before: a position on the
/// screen, not text.
fn spacer(readable: &str, raw: &str) -> bool {
    blank(readable) && readable.chars().count() != raw.chars().count()
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

/// The character after a § that makes the two a colour or formatting code.
fn colour_code(c: char) -> bool {
    matches!(c.to_ascii_lowercase(), '0'..='9' | 'a'..='f' | 'k'..='o' | 'r')
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

#[cfg(test)]
mod tests {
    use azalea::FormattedText;
    use regex::Regex;
    use serde_json::{Value, json};

    use super::{Line, readable};

    fn line(component: Value) -> Line {
        Line::of(&serde_json::from_value::<FormattedText>(component).unwrap())
    }

    #[test]
    fn pieces_with_no_font_run_together_as_drawn() {
        let line = line(json!({"text": "Hello ", "extra": [{"text": "world", "color": "red"}, " and welcome"]}));
        assert_eq!(line.shown, "Hello world and welcome");
        assert_eq!(line.readable, "Hello world and welcome");
    }

    #[test]
    fn a_font_labels_its_piece_and_separates_the_rest() {
        let line = line(json!({"text": "", "extra": [
            {"text": "Mana", "font": "hyperfarm:hud/illageralt"},
            " ",
            {"text": "10/20", "color": "aqua"},
        ]}));
        assert_eq!(line.shown, "[hud/illageralt] Mana | 10/20");
        assert_eq!(line.readable, "Mana 10/20");
    }

    #[test]
    fn a_piece_that_is_only_glyphs_is_a_spacer_and_dropped() {
        let line = line(json!({"text": "", "extra": [
            {"text": "\u{E000}", "font": "mcagents:ui/background"},
            {"text": "1/2", "font": "mcagents:ui/page_6"},
        ]}));
        assert_eq!(line.shown, "[ui/page_6] 1/2");
        assert_eq!(line.readable, "1/2");
        assert_eq!(line.raw, "\u{E000}1/2");
    }

    #[test]
    fn a_leaf_that_was_only_ever_a_space_is_kept() {
        assert_eq!(line(json!(["Cleared 0", " ", "[Track]"])).shown, "Cleared 0 [Track]");
    }

    #[test]
    fn nothing_readable_shows_as_the_text_it_reads() {
        let line = line(json!({"text": "\u{E001}\u{E002}", "font": "hyperfarm:hud/bars"}));
        assert_eq!(line.shown, "\u{E001}\u{E002}");
        assert_eq!(line.readable, "");
    }

    #[test]
    fn a_font_is_labelled_without_its_namespace() {
        assert_eq!(line(json!({"text": "Quest", "font": "hyperfarm:sidebar/title"})).shown, "[sidebar/title] Quest");
        assert_eq!(line(json!({"text": "Quest", "font": "sidebar/title"})).shown, "[sidebar/title] Quest");
    }

    /// A component built from a string keeps its codes as text, the way an owner named "§7" does.
    #[test]
    fn colour_codes_go_with_the_glyphs() {
        assert_eq!(readable("§7Harvest §Awheat\u{E000} §x"), "Harvest wheat §x");
        assert_eq!(readable("§8"), "");

        let line = Line::of(&FormattedText::from("§7Harvest wheat"));
        assert_eq!(line.raw, "§7Harvest wheat");
        assert_eq!(line.readable, "Harvest wheat");
        assert_eq!(line.shown, "Harvest wheat");
        assert_eq!(Line::of(&FormattedText::from("§8")).shown, "§8");
    }

    #[test]
    fn a_pattern_matches_any_of_the_three_wordings() {
        let line = line(json!({"text": "", "extra": [
            {"text": "\u{E000}", "font": "mcagents:ui/background"},
            {"text": "1/2", "font": "mcagents:ui/page_6"},
        ]}));
        assert!(line.matches(&Regex::new(r"^\[ui/page_6\] 1/2$").unwrap()));
        assert!(line.matches(&Regex::new("^1/2$").unwrap()));
        assert!(line.matches(&Regex::new("^\u{E000}1/2$").unwrap()));
        assert!(!line.matches(&Regex::new("^2/2$").unwrap()));
    }
}
