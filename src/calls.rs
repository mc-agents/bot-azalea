use std::future::Future;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::warn;

use crate::bot::Bot;

/// What a call comes to: a sentence, and for a structured tool the DTO the server words it from.
pub struct Answer {
    pub text: String,
    pub data: Option<Value>,
}

impl Answer {
    pub fn text(text: impl Into<String>) -> Answer {
        Answer { text: text.into(), data: None }
    }

    pub fn data(text: impl Into<String>, data: Value) -> Answer {
        Answer { text: text.into(), data: Some(data) }
    }
}

/// A call that did not work, in the classes docs/bot-protocol.md gives, because the class is what
/// decides what the server does with the session.
pub struct Failure {
    pub class: &'static str,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl Failure {
    pub fn refused(code: &str, message: impl Into<String>) -> Failure {
        Failure { class: "tool", code: code.into(), message: message.into(), retryable: false }
    }

    pub fn bad_args(message: impl Into<String>) -> Failure {
        Failure { class: "args", code: "BAD_ARGS".into(), message: message.into(), retryable: false }
    }

    pub fn not_in_game() -> Failure {
        Failure {
            class: "tool",
            code: "NOT_IN_GAME".into(),
            message: "the bot is not in a world".into(),
            retryable: true,
        }
    }

    pub fn unsupported(tool: &str) -> Failure {
        Failure {
            class: "unsupported",
            code: "NOT_IMPLEMENTED".into(),
            message: format!("{tool} is not implemented by the azalea bot"),
            retryable: false,
        }
    }
}

pub type Outcome = Result<Answer, Failure>;

/// Run one call to exactly one result.
///
/// The work, the deadline and a cancel race, and whichever finishes first is the answer: the other
/// two are dropped with the task, so there is no second path that could send another result for
/// the same id. That is the first invariant, held by construction rather than by a flag checked in
/// every tool.
pub fn run(bot: &Rc<Bot>, id: Value, name: String, deadline: Duration, work: impl Future<Output = Outcome> + 'static) {
    let key = id.to_string();
    let (cancel, cancelled) = oneshot::channel();

    if bot.calls.borrow().contains_key(&key) {
        warn!("a second call with id {key} while the first is in flight; ignoring it");
        return;
    }
    bot.calls.borrow_mut().insert(key.clone(), Some(cancel));

    let bot = bot.clone();
    tokio::task::spawn_local(async move {
        let started = Instant::now();
        let outcome = tokio::select! {
            outcome = work => outcome,
            _ = tokio::time::sleep(deadline) => Err(Failure {
                class: "timeout",
                code: "DEADLINE".into(),
                message: format!("{name} did not finish within {}ms", deadline.as_millis()),
                retryable: true,
            }),
            reason = cancelled => Err(Failure {
                class: "cancelled",
                code: "CANCELLED".into(),
                message: reason.unwrap_or_else(|_| "cancelled".into()),
                retryable: false,
            }),
        };

        /* Gone from the table means the link dropped under it, and there is nobody to answer. */
        if bot.calls.borrow_mut().remove(&key).is_none() {
            return;
        }
        bot.send(&result(id, started, outcome));
    });
}

pub fn cancel(bot: &Bot, id: &Value, reason: String) {
    /* An unknown id is a call that already answered: racing a completing call is normal. */
    if let Some(cancel) = bot.calls.borrow_mut().get_mut(&id.to_string()).and_then(Option::take) {
        let _ = cancel.send(reason);
    }
}

fn result(id: Value, started: Instant, outcome: Outcome) -> Value {
    let elapsed = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(answer) => {
            let mut result = json!({"t": "result", "id": id, "ok": true, "text": answer.text, "elapsedMs": elapsed});
            if let Some(data) = answer.data {
                result["data"] = data;
            }
            result
        }
        Err(failure) => json!({
            "t": "result",
            "id": id,
            "ok": false,
            "text": failure.message,
            "error": {
                "class": failure.class,
                "code": failure.code,
                "message": failure.message,
                "retryable": failure.retryable,
            },
            "elapsedMs": elapsed,
        }),
    }
}
