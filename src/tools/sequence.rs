use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use super::args::{integer, text};
use super::command::slashed;
use super::input::{self, PressArgs, TICK_MS, Watch};
use super::windows::{self, ClickArgs};
use super::{Tool, alive, in_world, tick};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};
use crate::text::Line;

enum Step {
    Press(PressArgs),
    Click(ClickArgs),
    Command(String),
    Wait(u32),
    WaitFor(Watch),
}

impl Step {
    fn kind(&self) -> &'static str {
        match self {
            Step::Press(_) => "press",
            Step::Click(_) => "click",
            Step::Command(_) => "command",
            Step::Wait(_) => "wait",
            Step::WaitFor(_) => "waitFor",
        }
    }

    /// The step in words, which is what names it once it has been refused or cut short and has no
    /// answer of its own to be named by.
    fn asked(&self) -> String {
        match self {
            Step::Press(press) => {
                let mut asked = match press.slot {
                    Some(slot) => format!("press hotbar {slot}"),
                    None => format!("press {}", press.key()),
                };
                if press.hold > 1 {
                    asked.push_str(&format!(" for {} ticks", press.hold));
                }
                asked
            }
            Step::Click(click) if click.mode != "click" => {
                let mut asked = format!("{} slot {}", click.mode, click.slot);
                if click.hotbar != 0 {
                    asked.push_str(&format!(" with hotbar {}", click.hotbar));
                }
                asked
            }
            Step::Click(click) => {
                let mut asked = format!("click slot {}", click.slot);
                if click.button == "right" {
                    asked.push_str(" with the right button");
                }
                if click.shift {
                    asked.push_str(" with shift");
                }
                asked
            }
            Step::Command(command) => format!("command {command}"),
            Step::Wait(1) => "wait 1 tick".to_owned(),
            Step::Wait(ticks) => format!("wait {ticks} ticks"),
            Step::WaitFor(watch) => format!("wait for /{}/ on {}", watch.source, watch.feed),
        }
    }

    /// The least the step takes, in ticks: a press is down for holdTicks and up for one, a click
    /// is sent on one tick, and a command or a line that is already there takes none.
    fn least_ticks(&self) -> u64 {
        match self {
            Step::Press(press) => u64::from(press.hold) + 1,
            Step::Click(_) => 1,
            Step::Wait(ticks) => u64::from(*ticks),
            Step::Command(_) | Step::WaitFor(_) => 0,
        }
    }
}

const KINDS: [&str; 5] = ["press", "click", "command", "wait", "waitFor"];

/// The step at `index`, which is exactly one of the kinds; the fields that shape another kind are
/// filled in by mcp-server whether or not the caller gave them, so they are not a second kind.
fn parse(step: &Value, index: usize, timeout: u64) -> Result<Step, Failure> {
    let number = index + 1;
    let named: Vec<&str> = KINDS.into_iter().filter(|kind| !step[*kind].is_null()).collect();
    let kind = match named[..] {
        [kind] => kind,
        [] => return Err(Failure::refused("BAD_STEP", format!("step {number} names none of press, click, command, wait or waitFor"))),
        [first, second, ..] => return Err(Failure::refused("BAD_STEP", format!("step {number} names both {first} and {second}"))),
    };

    let parsed = match kind {
        "press" => {
            if step["press"] == "hotbar" && step["slot"].is_null() {
                return Err(Failure::refused("NO_SLOT", format!("step {number}: hotbar needs slot")));
            }
            PressArgs::parse(&json!({
                "key": step["press"],
                "slot": step["slot"],
                "holdTicks": step["holdTicks"],
                "repeat": 1,
                "intervalTicks": 1,
                "after": null,
                "until": null,
                "timeoutMs": timeout,
            }))
            .map(Step::Press)
        }
        "click" => ClickArgs::parse(&json!({
            "slot": step["click"],
            "outside": false,
            "button": step["button"],
            "shift": step["shift"],
            "mode": step["mode"],
            "hotbar": step["hotbar"],
        }))
        .map(Step::Click),
        "command" => text(step, "command").map(|command| Step::Command(slashed(command))),
        "wait" => integer(step, "wait", 1).map(|ticks| Step::Wait(ticks as u32)),
        _ => Watch::parse(&json!({"feed": step["feed"], "pattern": step["waitFor"]})).map(Step::WaitFor),
    };
    parsed.map_err(|failure| Failure { message: format!("step {number}: {}", failure.message), ..failure })
}

fn steps(args: &Value, timeout: u64) -> Result<Vec<Step>, Failure> {
    let given = args["steps"].as_array().filter(|steps| !steps.is_empty());
    let given = given.ok_or_else(|| Failure::bad_args("expected a non-empty array of steps"))?;
    let steps = given.iter().enumerate().map(|(index, step)| parse(step, index, timeout)).collect::<Result<Vec<Step>, Failure>>()?;

    let least = steps.iter().map(Step::least_ticks).sum::<u64>() * TICK_MS;
    if least > timeout {
        return Err(Failure::refused(
            "TOO_LONG",
            format!("the {} steps take at least {least}ms, longer than timeoutMs ({timeout}ms).", steps.len()),
        ));
    }
    Ok(steps)
}

/// The lines shown since the sequence began, kept for a waitFor to look back over: a reply the
/// server sends while the step before is still settling has arrived before the waitFor starts.
///
/// They are taken off the feed as they come, on a task of their own: the feed keeps the last 256,
/// which an action bar drawn every tick fills in thirteen seconds, and a step can hold the
/// sequence for a minute.
struct Shown {
    lines: Rc<RefCell<Vec<(&'static str, Line)>>>,
    arrived: Rc<Notify>,
    collector: JoinHandle<()>,
    /// How many lines there were when the step before started. A waitFor looks back that far.
    mark: usize,
    /// One past the line the last waitFor took, so two waits for the same words take two lines.
    consumed: usize,
}

impl Shown {
    fn listen(bot: &Bot) -> Shown {
        let lines = Rc::new(RefCell::new(Vec::new()));
        let arrived = Rc::new(Notify::new());
        let mut feed = bot.shown.subscribe();
        let collector = tokio::task::spawn_local({
            let (lines, arrived) = (lines.clone(), arrived.clone());
            async move {
                loop {
                    match feed.recv().await {
                        Ok(line) => {
                            lines.borrow_mut().push(line);
                            arrived.notify_one();
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return,
                    }
                }
            }
        });
        Shown { lines, arrived, collector, mark: 0, consumed: 0 }
    }

    /// A step starts: where a waitFor starting now looks from, with this start kept for the next one.
    fn begin(&mut self) -> usize {
        let from = self.mark.max(self.consumed);
        self.mark = self.lines.borrow().len();
        from
    }

    async fn wait_for(&mut self, bot: &Bot, watch: &Watch, from: usize, started: Instant) -> Result<Value, Failure> {
        loop {
            let from = from.max(self.consumed);
            let found = {
                let lines = self.lines.borrow();
                lines[from..]
                    .iter()
                    .position(|(kind, line)| watch.matches(kind, line))
                    .map(|found| (from + found, lines[from + found].1.shown.clone()))
            };
            if let Some((at, shown)) = found {
                self.consumed = at + 1;
                let mut matched = watch.describe(Some(&shown));
                matched["waitedMs"] = json!(started.elapsed().as_millis() as u64);
                return Ok(matched);
            }
            tokio::select! {
                () = self.arrived.notified() => {}
                () = tick(bot) => in_world(bot, |_| ())?,
            }
        }
    }
}

impl Drop for Shown {
    fn drop(&mut self) {
        self.collector.abort();
    }
}

/// One step, made; what comes back goes under the step's kind in its record.
async fn run(bot: &Bot, step: Step, shown: &mut Shown, from: usize, started_tick: u64, tick0: u64) -> Result<Value, Failure> {
    match step {
        Step::Press(press) => {
            input::game_takes_keys(bot)?;
            /* The sequence's timeout is what cuts a press short, so the press has no deadline of its own. */
            input::press(bot, press, None).await
        }
        Step::Click(click) => {
            let clicked = windows::click_step(bot, &click).await?;
            /* The answer lands mid-tick; the step ends with the tick it landed on, as the other kind of bot's does. */
            tick(bot).await;
            Ok(clicked)
        }
        Step::Command(command) => {
            in_world(bot, |game| game.client.write_command_packet(&command[1..]))?;
            Ok(json!(command))
        }
        Step::Wait(ticks) => {
            let until = started_tick + u64::from(ticks);
            while bot.ticks.borrow().saturating_sub(tick0) < until {
                in_world(bot, |_| ())?;
                tick(bot).await;
            }
            Ok(json!(ticks))
        }
        Step::WaitFor(watch) => shown.wait_for(bot, &watch, from, Instant::now()).await,
    }
}

/// Inputs made one after another inside the bot, each on the client tick the one before ended on,
/// so that what lies between two of them is the ticks asked for and not a round trip each.
///
/// A step the game refuses ends the sequence with the steps that ran still answered: what they
/// did is what the caller wanted the sequence for. So does timeoutMs running out, with the step it
/// ran out in cut short; a key held then is let go of as the press is dropped.
pub const RUN_INPUTS: Tool = Tool {
    name: "run-inputs",
    run: |bot, args| {
        Box::pin(async move {
            let timeout = integer(&args, "timeoutMs", 10_000)? as u64;
            let steps = steps(&args, timeout)?;
            alive(&bot, |_| ())?;

            let total = steps.len();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
            let tick0 = *bot.ticks.borrow();
            let mut shown = Shown::listen(&bot);
            let mut records = Vec::with_capacity(total);
            let mut ran = 0;
            let mut stopped = "done";
            let mut ended_tick = 0;

            for step in steps {
                let from = shown.begin();
                let (kind, asked) = (step.kind(), step.asked());
                let started_tick = ended_tick;

                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                let outcome = if left.is_zero() {
                    None
                } else {
                    tokio::time::timeout(left, run(&bot, step, &mut shown, from, started_tick, tick0)).await.ok()
                };
                ended_tick = bot.ticks.borrow().saturating_sub(tick0);

                let mut record = json!({
                    "kind": kind,
                    "asked": asked,
                    "startedTick": started_tick,
                    "endedTick": ended_tick,
                    "press": null,
                    "click": null,
                    "command": null,
                    "wait": null,
                    "waitFor": null,
                    "error": null,
                });
                match outcome {
                    Some(Ok(did)) => {
                        record[kind] = did;
                        ran += 1;
                    }
                    Some(Err(refusal)) => {
                        record["error"] = json!({"code": refusal.code, "message": refusal.message});
                        stopped = "refused";
                    }
                    None => {
                        record["error"] = json!({"code": "TIMEOUT", "message": format!("timeoutMs ({timeout}) ran out during this step")});
                        stopped = "timeout";
                    }
                }
                records.push(record);
                if stopped != "done" {
                    break;
                }
            }

            Ok(Answer::data(
                format!("ran {ran} of {total} step(s), {stopped}"),
                json!({"asked": total, "ran": ran, "ticks": ended_tick, "stopped": stopped, "steps": records}),
            ))
        })
    },
};

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Step, parse, steps};
    use crate::calls::Failure;

    /// A step as mcp-server sends one: every field present, the ones not given at their defaults.
    fn step(given: Value) -> Value {
        let mut step = json!({
            "press": null, "slot": null, "holdTicks": 1, "click": null, "button": "left", "shift": false,
            "mode": "click", "hotbar": null, "command": null, "wait": null, "waitFor": null, "feed": "actionBar",
        });
        for (name, value) in given.as_object().unwrap() {
            step[name] = value.clone();
        }
        step
    }

    fn asked(given: Value) -> String {
        parse(&step(given), 0, 10_000).unwrap_or_else(|failure| panic!("{}", failure.message)).asked()
    }

    fn refusal(given: Value, index: usize) -> Failure {
        match parse(&step(given), index, 10_000) {
            Ok(step) => panic!("{} was not refused", step.asked()),
            Err(failure) => failure,
        }
    }

    #[test]
    fn a_step_is_worded_as_it_was_asked() {
        assert_eq!(asked(json!({"press": "jump"})), "press jump");
        assert_eq!(asked(json!({"press": "hotbar", "slot": 0})), "press hotbar 0");
        assert_eq!(asked(json!({"press": "use", "holdTicks": 40})), "press use for 40 ticks");
        assert_eq!(asked(json!({"click": 13})), "click slot 13");
        assert_eq!(asked(json!({"click": 13, "button": "right", "shift": true})), "click slot 13 with the right button with shift");
        assert_eq!(asked(json!({"click": 13, "mode": "swap-hotbar", "hotbar": 2})), "swap-hotbar slot 13 with hotbar 2");
        assert_eq!(asked(json!({"click": 5, "mode": "throw-one"})), "throw-one slot 5");
        assert_eq!(asked(json!({"command": "fixture pling BOT"})), "command /fixture pling BOT");
        assert_eq!(asked(json!({"command": "/fixture pling BOT"})), "command /fixture pling BOT");
        assert_eq!(asked(json!({"wait": 1})), "wait 1 tick");
        assert_eq!(asked(json!({"wait": 20})), "wait 20 ticks");
        assert_eq!(asked(json!({"waitFor": "Fine day", "feed": "title"})), "wait for /Fine day/ on title");
    }

    /// The slot a press names is the hotbar key's alone, as press-input takes it.
    #[test]
    fn a_slot_given_to_another_key_is_not_worded() {
        assert_eq!(asked(json!({"press": "jump", "slot": 3})), "press jump");
    }

    /// A slash is put in front of a command given without one, and nothing is taken off one that
    /// has its own: a WorldEdit command starts with two, as run-command sends it.
    #[test]
    fn a_command_keeps_the_slashes_it_was_given() {
        for (given, sent) in [("say hi", "/say hi"), ("/say hi", "/say hi"), ("//set stone", "//set stone")] {
            match parse(&step(json!({"command": given})), 0, 10_000) {
                Ok(Step::Command(command)) => assert_eq!(command, sent),
                _ => panic!("not a command"),
            }
        }
    }

    #[test]
    fn a_step_naming_no_kind_or_two_is_refused_by_its_number() {
        let none = refusal(json!({}), 2);
        assert_eq!(none.code, "BAD_STEP");
        assert_eq!(none.message, "step 3 names none of press, click, command, wait or waitFor");

        let both = refusal(json!({"press": "jump", "click": 13}), 2);
        assert_eq!(both.code, "BAD_STEP");
        assert_eq!(both.message, "step 3 names both press and click");
    }

    #[test]
    fn hotbar_without_a_slot_is_refused() {
        let refused = refusal(json!({"press": "hotbar"}), 1);
        assert_eq!(refused.code, "NO_SLOT");
        assert_eq!(refused.message, "step 2: hotbar needs slot");
    }

    #[test]
    fn a_pattern_that_is_not_one_is_refused_with_its_step() {
        let refused = refusal(json!({"waitFor": "("}), 1);
        assert_eq!(refused.code, "BAD_PATTERN");
        assert!(refused.message.starts_with("step 2: \"(\" is not a valid regular expression"), "{}", refused.message);
    }

    /// A click shaped wrongly is click-slot's to refuse, when the step is made in the window open
    /// then: the steps before it still run, as they do on the other kind of bot.
    #[test]
    fn a_click_shaped_wrongly_is_a_step_until_it_is_made() {
        assert_eq!(asked(json!({"click": 13, "mode": "throw-one", "shift": true})), "throw-one slot 13");
        assert_eq!(asked(json!({"click": 13, "mode": "throw-one", "hotbar": 2})), "throw-one slot 13 with hotbar 2");
    }

    #[test]
    fn steps_that_take_longer_than_the_timeout_are_refused_before_any_is_made() {
        let args = json!({"steps": [step(json!({"press": "use", "holdTicks": 40})), step(json!({"wait": 20})), step(json!({"click": 1}))], "timeoutMs": 3000});
        let refused = steps(&args, 3000).err().expect("refused");
        assert_eq!(refused.code, "TOO_LONG");
        assert_eq!(refused.message, "the 3 steps take at least 3100ms, longer than timeoutMs (3000ms).");

        assert_eq!(steps(&args, 3100).map(|steps| steps.len()).ok(), Some(3));
    }

    #[test]
    fn an_empty_sequence_is_refused() {
        assert_eq!(steps(&json!({"steps": []}), 10_000).err().map(|failure| failure.code), Some("BAD_ARGS".to_owned()));
    }
}
