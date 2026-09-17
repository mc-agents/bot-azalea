//! The block entities a vanilla client keeps, and the two editors it opens on them.
//!
//! azalea decodes the block entities in every chunk and in every update to one, and keeps none of
//! them. A sign's text is only in those, and so is a command block's command, and a client edits
//! both on screens built from what it was sent: a sign editor starts from the text already on the
//! sign, and the command block editor waits for the block's command before it lets anything be done.

use std::collections::HashMap;

use azalea::BlockPos;
use azalea::protocol::packets::game::s_set_command_block::Mode;
use serde_json::Value;

/// A block entity as the server last sent it, and the block it was sent for: a block entity outlives
/// its block in nothing the client draws, and one on a position whose block has since changed is gone.
pub struct Tracked {
    pub kind: String,
    pub block: Option<String>,
    pub data: Value,
}

#[derive(Default)]
pub struct BlockEntities {
    by_position: HashMap<BlockPos, Tracked>,
}

impl BlockEntities {
    pub fn keep(&mut self, at: BlockPos, tracked: Tracked) {
        self.by_position.insert(at, tracked);
    }

    /// A chunk arrives whole, so what was kept for it before is replaced rather than added to.
    pub fn forget_chunk(&mut self, x: i32, z: i32) {
        self.by_position
            .retain(|at, _| at.x.div_euclid(16) != x || at.z.div_euclid(16) != z);
    }

    pub fn clear(&mut self) {
        self.by_position.clear();
    }

    /// The block entity on a position, while the block it came with is still there.
    pub fn at(&self, at: BlockPos, block: &str) -> Option<&Tracked> {
        self.by_position
            .get(&at)
            .filter(|tracked| tracked.block.as_deref().is_none_or(|kept| kept == block))
    }
}

/// A sign's two faces, as their lines read and as the components they were written as.
pub fn sign_face(data: &Value, face: &str) -> (Vec<String>, Vec<Value>) {
    let messages: Vec<Value> = match data.get(face).and_then(|text| text.get("messages")) {
        Some(Value::Array(lines)) => lines.clone(),
        _ => Vec::new(),
    };
    let mut components: Vec<Value> = messages.into_iter().take(4).collect();
    components.resize(4, Value::String(String::new()));
    let lines = components.iter().map(crate::dialog::flatten).collect();
    (lines, components)
}

pub fn is_sign(kind: &str) -> bool {
    matches!(kind.strip_prefix("minecraft:").unwrap_or(kind), "sign" | "hanging_sign")
}

/// The sign editor the server opened on one face of a sign.
pub struct SignEditor {
    pub at: BlockPos,
    pub front: bool,
    pub hanging: bool,
    pub lines: [String; 4],
}

impl SignEditor {
    pub fn title(&self) -> &'static str {
        if self.hanging {
            "Edit Hanging Sign Message"
        } else {
            "Edit Sign Message"
        }
    }

    /// `SignBlockEntity.getMaxTextLineWidth`, in the pixels the client's font measures.
    pub fn line_width(&self) -> u32 {
        if self.hanging { 60 } else { 90 }
    }
}

/// The command block editor. It opens on the client before the server has sent the block's command,
/// and until that arrives its controls are off: a command typed then is written over when it lands.
pub struct CommandEditor {
    pub at: BlockPos,
    pub loaded: bool,
    pub command: String,
    pub previous_output: String,
    pub track_output: bool,
    pub mode: Mode,
    pub conditional: bool,
    pub automatic: bool,
}

impl CommandEditor {
    pub fn new(at: BlockPos) -> CommandEditor {
        CommandEditor {
            at,
            loaded: false,
            command: String::new(),
            previous_output: "-".into(),
            track_output: true,
            mode: Mode::Redstone,
            conditional: false,
            automatic: false,
        }
    }

    /// Filled from the block's data and its block, as `CommandBlockEditScreen.updateGui` fills it.
    pub fn load(&mut self, data: &Value, block: &str, conditional: bool) {
        self.command = data
            .get("Command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.track_output = flag(data.get("TrackOutput"), true);
        self.automatic = flag(data.get("auto"), false);
        self.previous_output = match data.get("LastOutput") {
            Some(output) if self.track_output => crate::dialog::flatten(output),
            _ => "-".into(),
        };
        self.mode = match block.strip_prefix("minecraft:").unwrap_or(block) {
            "chain_command_block" => Mode::Sequence,
            "repeating_command_block" => Mode::Auto,
            _ => Mode::Redstone,
        };
        self.conditional = conditional;
        self.loaded = true;
    }

    /// The buttons in the order the screen adds them, each under the words it shows.
    pub fn buttons(&self) -> [String; 6] {
        [
            if self.track_output { "O" } else { "X" }.into(),
            match self.mode {
                Mode::Sequence => "Chain",
                Mode::Auto => "Repeat",
                Mode::Redstone => "Impulse",
            }
            .into(),
            if self.conditional {
                "Conditional"
            } else {
                "Unconditional"
            }
            .into(),
            if self.automatic {
                "Always Active"
            } else {
                "Needs Redstone"
            }
            .into(),
            "Done".into(),
            "Cancel".into(),
        ]
    }

    /// A press on the button at an index. Every cycle moves one value on, wrapping, as a click does.
    pub fn press(&mut self, index: usize) {
        match index {
            0 => self.track_output = !self.track_output,
            1 => {
                self.mode = match self.mode {
                    Mode::Sequence => Mode::Auto,
                    Mode::Auto => Mode::Redstone,
                    Mode::Redstone => Mode::Sequence,
                }
            }
            2 => self.conditional = !self.conditional,
            3 => self.automatic = !self.automatic,
            _ => {}
        }
    }
}

fn flag(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_i64().is_some_and(|number| number != 0),
        _ => fallback,
    }
}

/// How wide a string draws in the client's default font, in pixels, advance included.
///
/// An approximation of the glyph widths in the default font's ASCII sheet, which is what a sign's
/// line limit is measured against. Wrong by a pixel or two on characters outside it, and a line a
/// pixel from the limit can take one character more or fewer than the other kind of bot's.
pub fn text_width(text: &str) -> u32 {
    text.chars()
        .map(|character| match character {
            ' ' | 't' | 'I' | '[' | ']' | '(' | ')' | '{' | '}' | '*' | '"' => 4,
            '!' | '\'' | ',' | '.' | ':' | ';' | '|' | 'i' => 2,
            'l' | '`' => 3,
            'f' | 'k' | '<' | '>' => 5,
            '@' | '~' => 7,
            character if u32::from(character) >= 0x2E80 => 9,
            _ => 6,
        })
        .sum()
}
