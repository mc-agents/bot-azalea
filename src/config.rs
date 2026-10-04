use std::env;
use std::path::PathBuf;
use std::time::Duration;

/// Where to dial and what to call ourselves, read from the environment the way
/// docs/bot-protocol.md lists it. There is no flag and no file: a pod spec already says all of
/// it, and a second way to say it is a second thing that can disagree. The one argument the binary
/// takes picks the bot or the login and configures neither.
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
    /// Which login the bot joins with, and so whether the username a connect asks for is the bot's
    /// to choose. Not in the protocol document: the server a bot links to has no say in which
    /// account it joins a game server with.
    pub auth: Auth,
}

/// How the bot proves to the game server that it is who it says it is.
///
/// Offline is the default and is what every bot did before there was a choice. It is also the only
/// one of the two that `docker compose up --scale bot=N` can mean anything with: replicas share
/// every environment variable, and one Minecraft account is one player.
pub enum Auth {
    Offline,
    Microsoft(Microsoft),
}

/// The Microsoft account a bot joins as, and the file azalea keeps its credential in.
pub struct Microsoft {
    /// The key the credential is cached under, which azalea asks for as the account's email. Any
    /// string does, but the login that writes the cache and the bot that reads it must agree on it.
    pub account: String,
    pub cache: PathBuf,
}

/// Where the credential goes when `BOT_AUTH_CACHE` does not say. A directory of its own, so the
/// volume that carries a login carries nothing else; not azalea's own `~/.minecraft/azalea-auth.json`,
/// which the uid the image runs as has no home directory for, and not the operator's `/work`, which
/// is an emptyDir and would throw the login away on the next restart.
const DEFAULT_CACHE: &str = "/auth/azalea-auth.json";

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Ok(Config {
            host: text("MCP_SERVER_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: number("MCP_SERVER_PORT").unwrap_or(8765),
            bot_name: text("BOT_NAME")
                .or_else(|| text("HOSTNAME"))
                .unwrap_or_else(|| "azalea".into()),
            link_token: text("BOT_LINK_TOKEN"),
            health_port: number("HEALTH_PORT").unwrap_or(8080),
            reconnect_min: Duration::from_millis(number("RECONNECT_MIN_MS").unwrap_or(500)),
            reconnect_max: Duration::from_millis(number("RECONNECT_MAX_MS").unwrap_or(15000)),
            auth: Auth::from_env()?,
        })
    }
}

impl Auth {
    fn from_env() -> Result<Auth, String> {
        match text("BOT_AUTH").as_deref().unwrap_or("offline") {
            "offline" => Ok(Auth::Offline),
            "microsoft" => Ok(Auth::Microsoft(Microsoft::from_env()?)),
            /*
            Refused rather than fallen back from. A bot that quietly joined offline against a server
            in online mode would be turned away by that server for having no session, which reads
            as the server's doing and sends whoever looks into it anywhere but at the letter they
            mistyped here.
            */
            other => Err(format!("BOT_AUTH is {other:?}; it is `offline` or `microsoft`")),
        }
    }
}

impl Microsoft {
    /// Read what a Microsoft login needs whatever `BOT_AUTH` says, because the login runs as its
    /// own one-shot: asking it to be told the mode as well is one more thing to get wrong.
    pub fn from_env() -> Result<Microsoft, String> {
        let Some(account) = text("BOT_AUTH_ACCOUNT") else {
            return Err("BOT_AUTH_ACCOUNT is unset: a Microsoft login needs the account it is for, which is the key its credential is cached under".into());
        };
        let cache = text("BOT_AUTH_CACHE").unwrap_or_else(|| DEFAULT_CACHE.to_owned());
        Ok(Microsoft {
            account,
            cache: cache.into(),
        })
    }
}

fn text(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn number<T: std::str::FromStr>(name: &str) -> Option<T> {
    text(name).and_then(|value| value.trim().parse().ok())
}
