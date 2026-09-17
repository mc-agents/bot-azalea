pub mod frame;

use std::rc::Rc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::bot::{Bot, KIND, MINECRAFT_VERSION, now_millis};
use crate::calls::{self, Failure};
use crate::catalog;
use crate::game;
use crate::tools;

/// Dial the MCP server, serve the link until it drops, and dial again. The bot is the one that
/// dials, so a pod finds the server on its own and a bot on a laptop can reach one in a cluster.
pub async fn run(bot: Rc<Bot>) {
    let mut backoff = bot.config.reconnect_min;

    loop {
        let address = format!("{}:{}", bot.config.host, bot.config.port);
        info!("dialling {address}");

        match TcpStream::connect(&address).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                backoff = bot.config.reconnect_min;
                serve(&bot, stream).await;
                warn!("the link to {address} dropped");
            }
            Err(failure) => warn!("could not reach {address}: {failure}"),
        }

        bot.detach();
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(bot.config.reconnect_max);
    }
}

async fn serve(bot: &Rc<Bot>, stream: TcpStream) {
    let (mut reader, mut writer) = stream.into_split();
    let (outbox, mut queued) = mpsc::unbounded_channel::<Vec<u8>>();

    let writing = tokio::task::spawn_local(async move {
        while let Some(frame) = queued.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    bot.attach(outbox);
    bot.send(&hello(bot));

    loop {
        match frame::read(&mut reader).await {
            Ok(Some(frame::Frame::Json(message))) => handle(bot, message),
            Ok(Some(frame::Frame::Unreadable(reason))) => {
                bot.log("warn", format!("MALFORMED_JSON from the server, ignored: {reason}"));
            }
            Ok(Some(frame::Frame::Other(kind))) => {
                bot.log("warn", format!("BAD_FRAME_TYPE {kind} from the server, ignored"));
            }
            Ok(None) => break,
            Err(failure) => {
                error!("reading the link failed: {failure}");
                break;
            }
        }
    }

    writing.abort();
}

fn hello(bot: &Bot) -> Value {
    let mut hello = json!({
        "t": "hello",
        "protocols": [catalog::PROTOCOL],
        "botName": bot.config.bot_name,
        "kind": KIND,
        "agentVersion": env!("CARGO_PKG_VERSION"),
        "mcVersion": MINECRAFT_VERSION,
        "catalogVersion": catalog::CATALOG_VERSION,
        "capabilities": tools::capabilities(),
        "features": [],
    });
    // Only when there is one: a server without a token ignores the field, and a server with one
    // refuses a hello that lacks it, so sending nothing is the same as sending the wrong thing.
    if let Some(token) = &bot.config.link_token {
        hello["linkToken"] = Value::String(token.clone());
    }
    hello
}

fn handle(bot: &Rc<Bot>, message: Value) {
    match message["t"].as_str().unwrap_or_default() {
        "helloOk" => {
            let feeds = message["events"].as_object().cloned().unwrap_or_else(Map::new);
            info!("linked as session {}", message["sessionId"].as_str().unwrap_or("?"));
            // A rejected tool is one the server will never call, and nothing else on this side
            // says so: the bot keeps implementing it and every call for it simply never comes.
            for rejected in message["rejectedTools"].as_array().into_iter().flatten() {
                warn!(
                    "the server rejected {}: {}",
                    rejected["tool"].as_str().unwrap_or("?"),
                    rejected["reason"].as_str().unwrap_or("no reason given")
                );
            }
            bot.accepted(feeds, message["repeatFlushMs"].as_u64());
            /* "idle" is the protocol's word for linked and in no world. */
            bot.status("idle", None);
        }
        "helloErr" | "fault" => error!(
            "the server closed the link: {} {}",
            message["code"].as_str().unwrap_or("?"),
            message["message"].as_str().unwrap_or_default()
        ),
        "connect" => game::connect(bot, &message),
        "disconnect" => game::disconnect(bot, &message),
        "call" => call(bot, &message),
        "cancel" => calls::cancel(
            bot,
            &message["id"],
            message["reason"].as_str().unwrap_or("cancelled").into(),
        ),
        "ping" => bot.send(&json!({
            "t": "pong",
            "nonce": message["nonce"],
            "ts": now_millis(),
            "busy": bot.busy(),
        })),
        "shutdown" => {
            info!("shutting down: {}", message["reason"].as_str().unwrap_or_default());
            std::process::exit(0);
        }
        other => warn!("unknown message type {other:?}"),
    }
}

fn call(bot: &Rc<Bot>, message: &Value) {
    let id = message["id"].clone();
    let name = message["tool"].as_str().unwrap_or_default().to_owned();
    let deadline = Duration::from_millis(message["deadlineMs"].as_u64().unwrap_or(30_000));
    let args = message["args"].clone();

    match tools::find(&name) {
        Some(tool) => calls::run(bot, id, name, deadline, (tool.run)(bot.clone(), args)),
        None => calls::run(bot, id, name.clone(), deadline, async move {
            Err(Failure::unsupported(&name))
        }),
    }
}
