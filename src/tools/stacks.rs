//! Stacks as the catalogue describes them. Every tool that names an item builds it here, so a field
//! one of them spells differently is a field the server cannot read in one place rather than six.

use azalea::inventory::ItemStack;
use azalea::inventory::components::{CustomName, ItemModel, ItemName, Lore, UseCooldown};
use serde_json::{Value, json};

use super::args::plain;
use super::text::component;

/// The registry path, which is how a sentence names an item: "iron_ingot", not "Iron Ingot".
pub fn name(stack: &ItemStack) -> &'static str {
    plain(stack.kind().to_str())
}

/// A carried stack in a numbered slot of the player's own inventory, with the ticks left on its
/// cooldown group when that is cooling down.
pub fn carried(stack: &ItemStack, slot: usize, cooldown: Option<u64>) -> Value {
    let mut entry = described(stack);
    entry["slot"] = json!(slot);
    entry["itemModel"] = model(stack);
    entry["cooldownTicks"] = json!(cooldown);
    entry
}

/// The cooldown group a stack uses, as the game picks it: the one its use_cooldown names, else the
/// item's own id. Every stack in a group cools down together.
pub fn cooldown_group(stack: &ItemStack) -> String {
    stack
        .get_component::<UseCooldown>()
        .and_then(|cooldown| cooldown.cooldown_group.as_ref().map(ToString::to_string))
        .unwrap_or_else(|| stack.kind().to_str().to_owned())
}

/// The item a display holds up: what it is, and the model it is drawn with.
pub fn shown(stack: &ItemStack) -> Value {
    if stack.is_empty() {
        return Value::Null;
    }
    let (label, label_component) = label(stack);
    json!({
        "name": name(stack),
        "count": stack.count(),
        "label": label,
        "labelComponent": label_component,
        "itemModel": model(stack),
    })
}

/// The model the client draws it with. Every item has one; a server that draws its own sets another.
fn model(stack: &ItemStack) -> Value {
    stack
        .get_component::<ItemModel>()
        .map_or(Value::Null, |model| json!(model.resource_location.to_string()))
}

/// A stack in a numbered slot of a window. Only ever called for one that holds something.
pub fn slot(stack: &ItemStack, index: usize) -> Value {
    let mut entry = described(stack);
    entry["slot"] = json!(index);
    entry
}

/// A stack that is not in a numbered slot: under the cursor, or on its way to the ground. Empty
/// travels as no stack at all rather than as a count of zero.
pub fn held(stack: &ItemStack) -> Value {
    if stack.is_empty() {
        Value::Null
    } else {
        described(stack)
    }
}

fn described(stack: &ItemStack) -> Value {
    let (label, label_component) = label(stack);

    /* A blank lore line is a line: it is where a menu puts its spacing. */
    let lines = stack
        .get_component::<Lore>()
        .map(|lore| lore.lines.clone())
        .unwrap_or_default();

    json!({
        "name": name(stack),
        "count": stack.count(),
        "label": label,
        "labelComponent": label_component,
        "lore": lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "loreComponents": lines.iter().map(component).collect::<Vec<_>>(),
    })
}

/// The name a server gave it, and the component that name was written as. A plugin draws a menu
/// out of custom-named items in the pack's own glyphs, and flattened here the icons would be gone
/// before mcp-server could tell a label from a spacer.
fn label(stack: &ItemStack) -> (Value, Value) {
    match stack.get_component::<CustomName>() {
        Some(custom) => (json!(custom.name.to_string()), component(&custom.name)),
        None => (Value::Null, Value::Null),
    }
}

/// Whether a stack answers to what a caller typed: the registry path or the name it is shown as.
pub fn matches(stack: &ItemStack, query: &str) -> bool {
    let needle = needle(query);
    name(stack).contains(&needle) || shown_as(stack).to_lowercase().contains(&needle)
}

/// What a caller typed, the way a registry name is compared: "minecraft:diamond" and "Diamond" are
/// both a diamond to somebody looking for one.
pub fn needle(query: &str) -> String {
    let lowered = query.trim().to_lowercase();
    plain(&lowered).to_owned()
}

/// The name on the tooltip: the custom name, else the item's own, in the client's language.
pub fn shown_as(stack: &ItemStack) -> String {
    if let Some(custom) = stack.get_component::<CustomName>() {
        return custom.name.to_string();
    }
    stack
        .get_component::<ItemName>()
        .map(|item| item.name.to_string())
        .unwrap_or_else(|| name(stack).to_owned())
}
