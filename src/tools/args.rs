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
    args[name]
        .as_str()
        .ok_or_else(|| Failure::bad_args(format!("expected a string for {name}")))
}

/// A registry name without its namespace, which is how the game's own lookups take it.
pub fn plain(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

pub fn point(position: BlockPos) -> Value {
    serde_json::json!({"x": position.x, "y": position.y, "z": position.z})
}

/// How a coordinate reads in a sentence, in the shape the other kind of bot prints it.
pub fn written(position: BlockPos) -> String {
    format!("({}, {}, {})", position.x, position.y, position.z)
}

/// An integer, or the fallback when the argument is absent or null. A whole number arrives as 3.0
/// as often as 3.
pub fn integer(args: &Value, name: &str, fallback: i64) -> Result<i64, Failure> {
    match &args[name] {
        Value::Null => Ok(fallback),
        value => value
            .as_i64()
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|number| number.fract() == 0.0)
                    .map(|number| number as i64)
            })
            .ok_or_else(|| Failure::bad_args(format!("expected an integer for {name}"))),
    }
}

pub fn boolean(args: &Value, name: &str, fallback: bool) -> Result<bool, Failure> {
    match &args[name] {
        Value::Null => Ok(fallback),
        value => value
            .as_bool()
            .ok_or_else(|| Failure::bad_args(format!("expected a boolean for {name}"))),
    }
}

pub fn text_or<'a>(args: &'a Value, name: &str, fallback: &'a str) -> Result<&'a str, Failure> {
    match &args[name] {
        Value::Null => Ok(fallback),
        _ => text(args, name),
    }
}
