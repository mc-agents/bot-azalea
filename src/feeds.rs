use azalea::chat::ChatPacket;
use serde_json::json;

use crate::bot::{Bot, now_millis};

/// A chat line, pushed as it arrives. Chat is never folded: the same line twice is information.
///
/// `source` is who produced it -- a player's name, or `system` for everything else -- and not
/// where the client drew it, which is what `kind` already says. The component goes as the server
/// sent it, as far as azalea kept it, and the server does the flattening.
pub fn chat(bot: &Bot, packet: &ChatPacket) {
    if !bot.wants("chat") {
        return;
    }
    let message = packet.message();
    let ts = now_millis();

    bot.send(&json!({
        "t": "event",
        "seq": bot.next_seq(),
        "kind": "chat",
        "source": packet.sender().unwrap_or_else(|| "system".into()),
        "text": message.to_string(),
        "component": serde_json::to_value(&message).unwrap_or_default(),
        "ts": ts,
        "firstTs": ts,
        "repeats": 1,
        "closed": false,
    }));
}
