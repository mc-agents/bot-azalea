use std::env;
use std::time::Duration;

/// Where to dial and what to call ourselves, read from the environment the way
/// docs/bot-protocol.md lists it. There is no flag and no file: a pod spec already says all of
/// it, and a second way to say it is a second thing that can disagree.
pub struct Config {
    pub host: String,
    pub port: u16,
    pub bot_name: String,
    /// What proves to the server that this is one of its own bots, when it has one to check
    /// against. Sent once in `hello` and kept out of every log line.
    pub link_token: Option<String>,
    pub health_port: u16,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            host: text("MCP_SERVER_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: number("MCP_SERVER_PORT").unwrap_or(8765),
            bot_name: text("BOT_NAME")
                .or_else(|| text("HOSTNAME"))
                .unwrap_or_else(|| "azalea".into()),
            link_token: text("BOT_LINK_TOKEN"),
            health_port: number("HEALTH_PORT").unwrap_or(8080),
            reconnect_min: Duration::from_millis(number("RECONNECT_MIN_MS").unwrap_or(500)),
            reconnect_max: Duration::from_millis(number("RECONNECT_MAX_MS").unwrap_or(15000)),
        }
    }
}

fn text(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn number<T: std::str::FromStr>(name: &str) -> Option<T> {
    text(name).and_then(|value| value.trim().parse().ok())
}
