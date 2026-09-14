//! What the open menu lets a player press, as opposed to click: an enchanting table's offers, a
//! stonecutter's results, a loom's patterns, a lectern's page turns, and a beacon's effects.
//!
//! None of these is a slot. The screen draws them from numbers the menu was sent and sends a button
//! number back, and the number means something different on every menu, so a caller chooses by name
//! and the number is worked out here from the same numbers the screen draws from.

use azalea::core::game_type::GameMode;
use azalea::entity::PlayerAbilities;
use azalea::inventory::components::{Dye, ProvidesBannerPatterns, WritableBookContent, WrittenBookContent};
use azalea::inventory::{ItemStack, Menu};
use azalea::local_player::LocalGameMode;
use azalea::protocol::packets::game::s_container_button_click::ServerboundContainerButtonClick;
use azalea::protocol::packets::game::s_set_beacon::ServerboundSetBeacon;
use azalea::registry::builtin::{ItemKind, MobEffect};
use azalea::registry::{DataRegistry, HolderSet, Registry};
use azalea::{Client, FormattedText, Identifier};
use azalea::container::ContainerHandleRef;
use regex::Regex;
use serde_json::{Value, json};
use std::io::Cursor;

use simdnbt::FromNbtTag;
use simdnbt::owned::{NbtCompound, NbtTag};

use super::args::{integer, plain};
use super::text::{component, translated};
use super::windows::{Window, kind, menu};
use super::{Tool, alive, in_world, stacks};
use crate::calls::{Answer, Failure};
use crate::menus::{Known, Menus, first_item};
use azalea::protocol::packets::game::c_update_recipes::SingleInputEntry;

const LECTERN_PREVIOUS: i32 = 1;
const LECTERN_NEXT: i32 = 2;
const LECTERN_TAKE: i32 = 3;
const LECTERN_JUMP: i32 = 100;

/// A beacon's effects by the pyramid level each needs; the last row is the secondary's.
const BEACON_EFFECTS: &[&[MobEffect]] =
    &[&[MobEffect::Speed, MobEffect::Haste], &[MobEffect::Resistance, MobEffect::JumpBoost], &[MobEffect::Strength], &[MobEffect::Regeneration]];
const SECONDARY_LEVELS: i32 = BEACON_EFFECTS.len() as i32;
const BEACON_PAYMENT: usize = 0;

struct Choice {
    button: i32,
    name: Option<String>,
    label: Option<String>,
    count: Option<i32>,
    levels: Option<i32>,
    lapis: Option<i32>,
    available: bool,
    selected: bool,
}

impl Choice {
    fn plain(button: i32, name: &str, available: bool) -> Choice {
        Choice { button, name: Some(name.to_owned()), label: None, count: None, levels: None, lapis: None, available, selected: false }
    }

    fn json(&self) -> Value {
        json!({
            "button": self.button,
            "name": self.name,
            "label": self.label,
            "count": self.count,
            "levels": self.levels,
            "lapis": self.lapis,
            "available": self.available,
            "selected": self.selected,
        })
    }

    /// How a refusal names it: the words a caller would type to choose it.
    fn spoken(&self) -> String {
        match (&self.label, &self.name) {
            (Some(label), _) => format!("\"{label}\""),
            (None, Some(name)) => format!("\"{name}\""),
            (None, None) => format!("button {}", self.button),
        }
    }
}

/// Everything a menu's options are read from, taken out of the ECS in one go.
struct Reading {
    window: Window,
    known: Known,
    stonecutter: Vec<SingleInputEntry>,
    /// The menu's data values, where the server has sent them. A selection nobody has made is -1 in
    /// the game, and a value that has not arrived reads as that rather than as the first option.
    data: Vec<Option<i32>>,
    level: i32,
    creative: bool,
    may_build: bool,
}

fn reading(client: &Client) -> Option<Reading> {
    let window = menu(client)?;
    let known = client.get_component::<Known>()?.clone();
    let (stonecutter, data) = {
        let menus = client.get_component::<Menus>()?;
        (menus.stonecutter.clone(), (0..10).map(|index| menus.value(window.id, index)).collect())
    };
    let mode = client.get_component::<LocalGameMode>().map(|mode| mode.current);
    Some(Reading {
        window,
        known,
        stonecutter,
        data,
        level: client.experience().level as i32,
        creative: client.get_component::<PlayerAbilities>().is_some_and(|abilities| abilities.instant_break),
        may_build: !matches!(mode, Some(GameMode::Adventure | GameMode::Spectator)),
    })
}

fn require(client: &Client) -> Result<Reading, Failure> {
    reading(client).ok_or_else(|| {
        Failure::refused(
            "NO_WINDOW",
            "No window is open. Run the command that opens the menu first, then use wait-for-window before pressing anything on it.",
        )
    })
}

impl Reading {
    fn data(&self, index: usize) -> i32 {
        self.data[index].unwrap_or(0)
    }

    fn selected(&self) -> i32 {
        self.data[0].unwrap_or(-1)
    }

    fn slot(&self, index: usize) -> ItemStack {
        self.window.menu.slot(index).cloned().unwrap_or_default()
    }

    fn choices(&self) -> Vec<Choice> {
        match self.window.menu {
            Menu::Enchantment { .. } => self.enchanting(),
            Menu::Stonecutter { .. } => self.stonecutter(),
            Menu::Loom { .. } => self.loom(),
            Menu::Lectern { .. } => self.lectern(),
            _ => Vec::new(),
        }
    }

    /// The three offers, from the costs and clues the server pushed into the menu. An offer with no
    /// cost is a slot the table has nothing in, not a free one.
    fn enchanting(&self) -> Vec<Choice> {
        let has_item = !self.slot(0).is_empty();
        let lapis_held = self.slot(1).count();

        (0..3)
            .filter(|button| self.data(*button) > 0)
            .map(|button| {
                let cost = self.data(button);
                let clue = self.data[4 + button].unwrap_or(-1);
                let level = self.data(7 + button);
                let lapis = button as i32 + 1;
                let entry = usize::try_from(clue).ok().and_then(|clue| self.known.entry("minecraft:enchantment", clue));

                Choice {
                    button: button as i32,
                    name: entry.map(|(id, _)| id.path().to_owned()),
                    label: entry.and_then(|(_, data)| enchantment_name(data.as_ref()?, level)),
                    count: None,
                    levels: Some(cost),
                    lapis: Some(lapis),
                    available: has_item && (self.creative || (lapis_held >= lapis && self.level >= cost)),
                    selected: false,
                }
            })
            .collect()
    }

    /// The results for what is in the input slot, in the order the server listed its recipes. The
    /// client is sent the list without the recipes, so the result on the button is all there is to
    /// name one by.
    fn stonecutter(&self) -> Vec<Choice> {
        let input = self.slot(0);
        if input.is_empty() {
            return Vec::new();
        }
        let selected = self.selected();

        self.stonecutter
            .iter()
            .filter(|entry| self.accepts(&entry.input.allowed, input.kind()))
            .filter_map(|entry| first_item(&entry.recipe.option_display, &self.known))
            .enumerate()
            .map(|(button, (item, count))| {
                let result = ItemStack::new(item, count);
                Choice {
                    button: button as i32,
                    name: Some(stacks::name(&result).to_owned()),
                    label: Some(stacks::shown_as(&result)),
                    count: Some(count),
                    levels: None,
                    lapis: None,
                    available: true,
                    selected: selected == button as i32,
                }
            })
            .collect()
    }

    fn accepts(&self, allowed: &HolderSet<ItemKind, Identifier>, item: ItemKind) -> bool {
        match allowed {
            HolderSet::Direct { contents } => contents.contains(&item),
            HolderSet::Named { key, .. } => self.known.tag("minecraft:item", key).contains(&(item.to_u32() as i32)),
        }
    }

    /// Every pattern the loom offers for the banner and dye in it, named by its own id and by what
    /// the banner's tooltip would call it in that dye.
    fn loom(&self) -> Vec<Choice> {
        let (banner, dye, pattern) = (self.slot(0), self.slot(1), self.slot(2));
        if banner.is_empty() || dye.is_empty() {
            return Vec::new();
        }
        let ids: Vec<i32> = if pattern.is_empty() {
            self.known.tag("minecraft:banner_pattern", &Identifier::new("minecraft:no_item_required")).to_vec()
        } else {
            match pattern.get_component::<ProvidesBannerPatterns>().map(|provided| provided.key.clone()) {
                Some(HolderSet::Direct { contents }) => contents.iter().map(|pattern| pattern.protocol_id() as i32).collect(),
                Some(HolderSet::Named { key, .. }) => self.known.tag("minecraft:banner_pattern", &key).to_vec(),
                None => Vec::new(),
            }
        };
        let color = dye
            .get_component::<Dye>()
            .and_then(|dye| serde_json::to_value(dye.color).ok())
            .and_then(|color| color.as_str().map(str::to_owned));
        let selected = self.selected();

        ids.iter()
            .enumerate()
            .map(|(button, id)| {
                let entry = usize::try_from(*id).ok().and_then(|id| self.known.entry("minecraft:banner_pattern", id));
                let key = entry.and_then(|(_, data)| data.as_ref()?.string("translation_key").map(|key| key.to_str().into_owned()));
                Choice {
                    button: button as i32,
                    name: entry.map(|(id, _)| id.path().to_owned()),
                    label: key.zip(color.as_ref()).map(|(key, color)| translated(&format!("{key}.{color}"))),
                    count: None,
                    levels: None,
                    lapis: None,
                    available: true,
                    selected: selected == button as i32,
                }
            })
            .collect()
    }

    /// Turning a page is a button the server answers, not a widget: the page moves when it says so.
    fn lectern(&self) -> Vec<Choice> {
        let page = self.data(0);
        let pages = page_count(&self.slot(0));
        vec![
            Choice::plain(LECTERN_PREVIOUS, "previous page", page > 0),
            Choice::plain(LECTERN_NEXT, "next page", page < pages - 1),
            Choice::plain(LECTERN_TAKE, "take book", self.may_build),
        ]
    }

    fn describe(&self) -> Value {
        let lectern = matches!(self.window.menu, Menu::Lectern { .. });
        /* A lectern is drawn by the book screen, which has no title of its own. */
        let title = if lectern { FormattedText::default() } else { self.window.title.clone() };

        json!({
            "title": title.to_string(),
            "titleComponent": component(&title),
            "type": kind(&self.window.menu),
            "options": self.choices().iter().map(Choice::json).collect::<Vec<_>>(),
            "page": if lectern { json!(self.data(0) + 1) } else { Value::Null },
            "pageCount": if lectern { json!(page_count(&self.slot(0))) } else { Value::Null },
            "beacon": if matches!(self.window.menu, Menu::Beacon { .. }) { self.beacon() } else { Value::Null },
        })
    }

    fn primary(&self) -> Option<MobEffect> {
        decode_effect(self.data(1))
    }

    fn secondary(&self) -> Option<MobEffect> {
        decode_effect(self.data(2))
    }

    /// The buttons in the order the screen lays them out: the primary rows, regeneration, then the
    /// upgrade button, which is the primary again and is only there once a primary is set.
    fn beacon(&self) -> Value {
        let levels = self.data(0);
        let payment = self.slot(BEACON_PAYMENT);
        let mut effects = Vec::new();

        for (row, effects_in_row) in BEACON_EFFECTS.iter().enumerate() {
            for effect in *effects_in_row {
                let primary = (row as i32) < SECONDARY_LEVELS - 1;
                effects.push(self.effect_choice(*effect, primary, row as i32 + 1, false));
            }
        }
        if let Some(primary) = self.primary() {
            effects.push(self.effect_choice(primary, false, SECONDARY_LEVELS, true));
        }

        json!({
            "levels": levels,
            "payment": if payment.is_empty() { Value::Null } else { json!(stacks::name(&payment)) },
            "effects": effects,
        })
    }

    fn effect_choice(&self, effect: MobEffect, primary: bool, levels: i32, upgrade: bool) -> Value {
        let current = if primary { self.primary() } else { self.secondary() };
        let label = effect_label(effect);
        json!({
            "name": plain(effect.to_str()),
            "label": if upgrade { format!("{label} II") } else { label },
            "slot": if primary { "primary" } else { "secondary" },
            "levels": levels,
            "available": self.data(0) >= levels,
            "selected": current == Some(effect),
        })
    }
}

/// An enchantment as the table's tooltip names it: its description, and the level after it unless
/// it only ever has the one.
fn enchantment_name(data: &NbtCompound, level: i32) -> Option<String> {
    let description = text_tag(data.get("description")?)?.to_string();
    let max_level = data.int("max_level").unwrap_or(1);

    if level != 1 || max_level != 1 {
        Some(format!("{description} {}", translated(&format!("enchantment.level.{level}"))))
    } else {
        Some(description)
    }
}

/// A component stored in registry data. azalea reads components from borrowed NBT only, so the tag
/// is written out and read back borrowed.
fn text_tag(tag: &NbtTag) -> Option<FormattedText> {
    let mut wrapper = NbtCompound::new();
    wrapper.insert("text", tag.clone());
    let mut bytes = Vec::new();
    wrapper.write(&mut bytes);
    let base = simdnbt::borrow::read_compound(&mut Cursor::new(&bytes)).ok()?;
    let compound: simdnbt::borrow::NbtCompound = (&base).into();
    FormattedText::from_nbt_tag(compound.get("text")?)
}

/// The pages a book on a lectern or in a hand has, whichever kind of book it is.
fn page_count(book: &ItemStack) -> i32 {
    if let Some(written) = book.get_component::<WrittenBookContent>() {
        return written.pages.len() as i32;
    }
    book.get_component::<WritableBookContent>().map_or(0, |writable| writable.pages.len() as i32)
}

/// A beacon's effect as its data value holds one: the registry id plus one, and 0 for none.
fn decode_effect(value: i32) -> Option<MobEffect> {
    u32::try_from(value - 1).ok().and_then(MobEffect::from_u32)
}

fn effect_label(effect: MobEffect) -> String {
    translated(&format!("effect.minecraft.{}", plain(effect.to_str())))
}

fn effect_named(effect: MobEffect) -> String {
    format!("{} [{}]", effect_label(effect), plain(effect.to_str()))
}

pub const READ_CONTAINER_OPTIONS: Tool = Tool {
    name: "read-container-options",
    run: |bot, _args| {
        Box::pin(async move {
            let window = in_world(&bot, |game| reading(&game.client).map(|reading| reading.describe()))?;
            Ok(Answer::data("read-container-options", json!({"window": window})))
        })
    },
};

pub const PRESS_CONTAINER_BUTTON: Tool = Tool {
    name: "press-container-button",
    run: |bot, args| {
        Box::pin(async move {
            let option = match &args["option"] {
                Value::Null => None,
                _ => Some(super::args::text(&args, "option")?.to_owned()),
            };
            let button = integer(&args, "button", -1)?;
            if option.is_none() == (button < 0) {
                return Err(Failure::bad_args("give either option, by name, or button, by number"));
            }

            let sentence = alive(&bot, |game| {
                let reading = require(&game.client)?;
                press(&game.client, &reading, option.as_deref(), button)
            })??;
            Ok(Answer::text(sentence))
        })
    },
};

fn press(client: &Client, reading: &Reading, option: Option<&str>, button: i64) -> Result<String, Failure> {
    let kind = kind(&reading.window.menu);
    let send = |button: i32| {
        client.write_packet(ServerboundContainerButtonClick { container_id: reading.window.id, button_id: button as u32 });
    };

    let Some(option) = option else {
        send(button as i32);
        return Ok(format!("Pressed button {button} on {kind}."));
    };

    if matches!(reading.window.menu, Menu::Lectern { .. })
        && let Some(page) = Regex::new(r"(?i)^page\s+(\d+)$").ok().and_then(|pattern| pattern.captures(option.trim()).map(|found| found[1].to_owned()))
    {
        let page: i32 = page.parse().unwrap_or(i32::MAX);
        let pages = page_count(&reading.slot(0));
        if page < 1 || page > pages {
            return Err(Failure::refused("NO_SUCH_PAGE", format!("the book on this lectern has pages 1-{pages}, and no page {page}")));
        }
        if page == reading.data(0) + 1 {
            return Ok(format!("The lectern is already open at page {page}."));
        }
        let button = LECTERN_JUMP + page - 1;
        send(button);
        return Ok(format!("Pressed \"page {page}\" (button {button}) on {kind}."));
    }

    let choices = reading.choices();
    let chosen = pick(&choices, option, reading, kind)?;

    if chosen.selected {
        return Ok(format!("{} is already selected on {kind}.", chosen.spoken()));
    }
    if !chosen.available {
        return Err(Failure::refused("OPTION_UNAVAILABLE", format!("{} cannot be pressed{}.", chosen.spoken(), why(chosen, reading))));
    }
    send(chosen.button);
    Ok(format!("Pressed {} (button {}) on {kind}.", chosen.spoken(), chosen.button))
}

/// Exact before partial, and a partial that matches more than one is refused. "Sharpness" is part
/// of both "Sharpness I" and "Sharpness III", and taking the first spends the levels on whichever the
/// table happened to list first.
fn pick<'a>(choices: &'a [Choice], wanted: &str, reading: &Reading, kind: &str) -> Result<&'a Choice, Failure> {
    let lowered = wanted.trim().to_lowercase();
    let equal = |text: &Option<String>| text.as_ref().is_some_and(|text| text.to_lowercase() == lowered);
    let contains = |text: &Option<String>| text.as_ref().is_some_and(|text| text.to_lowercase().contains(&lowered));

    if let Some(exact) = choices.iter().find(|choice| equal(&choice.label) || equal(&choice.name)) {
        return Ok(exact);
    }
    let partial: Vec<&Choice> = choices.iter().filter(|choice| contains(&choice.label) || contains(&choice.name)).collect();
    match partial.as_slice() {
        [only] => Ok(only),
        [] => Err(Failure::refused("NO_SUCH_OPTION", format!("no option matching \"{wanted}\" on {kind}; {}", offers(choices, reading)))),
        many => Err(Failure::refused(
            "AMBIGUOUS_OPTION",
            format!("\"{wanted}\" matches {} on {kind}; name one of them", list(many.iter().copied())),
        )),
    }
}

fn list<'a>(choices: impl Iterator<Item = &'a Choice>) -> String {
    choices.map(Choice::spoken).collect::<Vec<_>>().join(", ")
}

fn offers(choices: &[Choice], reading: &Reading) -> String {
    match reading.window.menu {
        Menu::Lectern { .. } => format!("it offers {}, and \"page 1\" to \"page {}\"", list(choices.iter()), page_count(&reading.slot(0))),
        Menu::CartographyTable { .. } => {
            "a cartography table has nothing to press: its result appears in slot 2 once both inputs are in, and click-slot takes it".to_owned()
        }
        _ if choices.is_empty() => "it offers nothing to press right now".to_owned(),
        _ => format!("it offers {}", list(choices.iter())),
    }
}

fn why(choice: &Choice, reading: &Reading) -> String {
    if let (Some(levels), Some(lapis)) = (choice.levels, choice.lapis) {
        if reading.slot(0).is_empty() {
            return ": there is nothing on the table to enchant".to_owned();
        }
        return format!(
            ": it costs {levels} levels and {lapis} lapis, and the bot has {} levels and {} lapis",
            reading.level,
            reading.slot(1).count()
        );
    }
    if matches!(reading.window.menu, Menu::Lectern { .. }) {
        return if choice.button == LECTERN_TAKE { ": the bot may not build here" } else { ": the book is already at that end" }.to_owned();
    }
    String::new()
}

/// Choose a beacon's effects and confirm them, as the confirm button does: both effects in one
/// packet, and the window closed in the same press. Left open, the window would describe the effects
/// from before until the server's data came back, and a second set from it would pay a second time.
pub const SET_BEACON_EFFECTS: Tool = Tool {
    name: "set-beacon-effects",
    run: |bot, args| {
        Box::pin(async move {
            let primary_name = super::args::text(&args, "primary")?.to_owned();
            let secondary_name = match &args["secondary"] {
                Value::Null => None,
                _ => Some(super::args::text(&args, "secondary")?.to_owned()),
            };

            let sentence = alive(&bot, |game| {
                let reading = require(&game.client)?;
                if !matches!(reading.window.menu, Menu::Beacon { .. }) {
                    return Err(Failure::refused(
                        "NOT_A_BEACON",
                        format!("the open window is {}, not a beacon", kind(&reading.window.menu)),
                    ));
                }

                let primary = effect(&primary_name)?;
                let secondary = secondary_name.as_deref().map(effect).transpose()?;
                check(&reading, primary, secondary)?;
                let payment = stacks::name(&reading.slot(BEACON_PAYMENT));

                game.client.write_packet(ServerboundSetBeacon {
                    primary: Some(primary.to_u32()),
                    secondary: secondary.map(|effect| effect.to_u32()),
                });
                ContainerHandleRef::new(reading.window.id, game.client.clone()).close();

                let second = match secondary {
                    None => " with no secondary effect".to_owned(),
                    Some(secondary) if secondary == primary => " at level II".to_owned(),
                    Some(secondary) => format!(" and its secondary effect to {}", effect_named(secondary)),
                };
                Ok(format!(
                    "Set the beacon's primary effect to {}{second}, paying one {payment}. The window is closed, as the beacon's confirm button closes it.",
                    effect_named(primary)
                ))
            })??;
            Ok(Answer::text(sentence))
        })
    },
};

/// An effect a beacon gives, by id or by the name the screen shows. Anything else is refused here,
/// because the server would store it as no effect at all and say nothing.
fn effect(wanted: &str) -> Result<MobEffect, Failure> {
    let needle = stacks::needle(wanted);
    BEACON_EFFECTS
        .iter()
        .flat_map(|row| row.iter())
        .find(|effect| plain(effect.to_str()) == needle || effect_label(**effect).eq_ignore_ascii_case(wanted.trim()))
        .copied()
        .ok_or_else(|| {
            let all: Vec<&str> = BEACON_EFFECTS.iter().flat_map(|row| row.iter()).map(|effect| plain(effect.to_str())).collect();
            Failure::refused("NO_SUCH_EFFECT", format!("a beacon gives none called \"{wanted}\"; it gives {}", all.join(", ")))
        })
}

fn levels_for(effect: MobEffect) -> i32 {
    BEACON_EFFECTS.iter().position(|row| row.contains(&effect)).map_or(i32::MAX, |row| row as i32 + 1)
}

/// Refuses, with the reason, the pair the confirm button could not have sent.
fn check(reading: &Reading, primary: MobEffect, secondary: Option<MobEffect>) -> Result<(), Failure> {
    let levels = reading.data(0);
    let refused = |message: String| Err(Failure::refused("EFFECT_UNAVAILABLE", message));
    let counted = |levels: i32| format!("{levels} {}", if levels == 1 { "level" } else { "levels" });
    let pyramid = |levels: i32| if levels == 0 { "has none under it".to_owned() } else { format!("has {}", counted(levels)) };

    if levels_for(primary) >= SECONDARY_LEVELS {
        let primaries: Vec<&str> =
            BEACON_EFFECTS[..BEACON_EFFECTS.len() - 1].iter().flat_map(|row| row.iter()).map(|effect| plain(effect.to_str())).collect();
        return refused(format!("{} is only ever a secondary effect; the primary is one of {}", effect_named(primary), primaries.join(", ")));
    }
    if levels_for(primary) > levels {
        return refused(format!(
            "{} needs a pyramid of {}, and this beacon's {}",
            effect_named(primary),
            counted(levels_for(primary)),
            pyramid(levels)
        ));
    }
    if secondary.is_some() && levels < SECONDARY_LEVELS {
        return refused(format!(
            "a secondary effect needs a pyramid of {}, and this beacon's {}; leave secondary out",
            counted(SECONDARY_LEVELS),
            pyramid(levels)
        ));
    }
    if let Some(secondary) = secondary
        && levels_for(secondary) < SECONDARY_LEVELS
        && secondary != primary
    {
        return refused(format!(
            "the secondary effect is regeneration or the primary again, which makes {} level II; {} is neither",
            effect_named(primary),
            effect_named(secondary)
        ));
    }
    if reading.slot(BEACON_PAYMENT).is_empty() {
        return Err(Failure::refused(
            "NO_PAYMENT",
            format!(
                "the beacon takes one iron ingot, gold ingot, emerald, diamond or netherite ingot for every change, and its payment slot (slot {BEACON_PAYMENT}) is empty; put one there with click-slot first"
            ),
        ));
    }
    Ok(())
}
