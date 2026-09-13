use azalea::BlockPos;
use serde_json::Value;

use crate::calls::Failure;

/// A block position from x, y and z. The server has already floored them, but a number is a number
/// on the wire, so a whole one arrives as 3.0 as often as 3.
pub fn position(args: &Value) -> Result<BlockPos, Failure> {
    let axis = |name: &str| {
        args[name]
            .as_f64()
            .map(|value| value.floor() as i32)
            .ok_or_else(|| Failure::bad_args(format!("expected a number for {name}")))
    };
    Ok(BlockPos::new(axis("x")?, axis("y")?, axis("z")?))
}

pub fn text<'a>(args: &'a Value, name: &str) -> Result<&'a str, Failure> {
    args[name].as_str().ok_or_else(|| Failure::bad_args(format!("expected a string for {name}")))
}

/// A registry name without its namespace, which is how the game's own lookups take it.
pub fn plain(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

pub fn point(position: BlockPos) -> Value {
    serde_json::json!({"x": position.x, "y": position.y, "z": position.z})
}
