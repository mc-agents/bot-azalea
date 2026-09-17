use azalea::block::BlockState;
use azalea::core::direction::Direction;
use azalea::entity::LookDirection;
use azalea::entity::inventory::Inventory;
use azalea::interact::BlockStatePredictionHandler;
use azalea::inventory::components::CustomName;
use azalea::mining::StopMiningBlockEvent;
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::protocol::packets::game::s_player_action::{Action, ServerboundPlayerAction};
use azalea::protocol::packets::game::s_use_item_on::BlockHit;
use azalea::protocol::packets::game::{ServerboundSwing, ServerboundUseItem, ServerboundUseItemOn};
use azalea::registry::builtin::BlockKind;
use azalea::{BlockPos, Client, Vec3};

use super::approach::Approach;
use super::args::{plain, position, text, written};
use super::stacks;
use super::{Tool, alive, tick};
use crate::bot::Bot;
use crate::calls::{Answer, Failure};
use crate::game::Game;

const TICK_MS: u64 = 50;

/// Long enough for a hand on stone, short enough to say so rather than sit on the deadline.
const DIG_PATIENCE_TICKS: u32 = 600;

/// What a creative client waits between blocks it breaks, and so between asking again for one that
/// did not go.
const DIG_RETRY_TICKS: u32 = 5;

/// The faces a block can be placed against, in the order they are tried after the caller's own.
const FACES: [Direction; 6] = [
    Direction::Down,
    Direction::Up,
    Direction::North,
    Direction::South,
    Direction::East,
    Direction::West,
];

pub const DIG_BLOCK: Tool = Tool {
    name: "dig-block",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;

            /* Nothing to dig is a state, not a refusal: asking twice should not fail the second time. */
            let state = alive(&bot, |game| block_at(game, at))?;
            if state.is_air() {
                return Ok(Answer::text(format!("Nothing to dig at {}.", written(at))));
            }
            let block = name(state);

            approach(&bot, at).await?;

            /*
            Breaking is not one packet. The client says it has started and keeps hitting until the
            server agrees the block is gone -- at once in creative, as long as the tool in hand takes
            in survival. Both look the same from here: keep going until the block is not there.
            */
            let digging = Digging(alive(&bot, |game| game.client.clone())?);
            let mut ticks = 0;
            loop {
                let (gone, mining) = alive(&bot, |game| (block_at(game, at).is_air(), game.client.is_mining()))?;
                if gone {
                    return Ok(Answer::text(format!("Dug {block} at {}.", written(at))));
                }
                if ticks > DIG_PATIENCE_TICKS {
                    return Err(Failure::refused(
                        "DIG_STUCK",
                        format!(
                            "{block} at {} did not break in {}s. Nothing in hand may be able to break it.",
                            written(at),
                            DIG_PATIENCE_TICKS / 20
                        ),
                    ));
                }
                /* Looked at, so the face azalea reports hitting is the one facing the bot. */
                digging.0.look_at(at.center());
                if !mining && ticks % DIG_RETRY_TICKS == 0 {
                    digging.0.start_mining(at);
                }
                tick(&bot).await;
                ticks += 1;
            }
        })
    },
};

/// A block being broken, abandoned when the call ends before it breaks. Left mid-swing, the client
/// goes on sending progress for a block nobody asked about any more.
struct Digging(Client);

impl Drop for Digging {
    fn drop(&mut self) {
        if self.0.is_mining() {
            self.0
                .ecs
                .write()
                .write_message(StopMiningBlockEvent { entity: self.0.entity });
        }
    }
}

pub const PLACE_BLOCK: Tool = Tool {
    name: "place-block",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;
            let preferred = face(text(&args, "faceDirection")?)?;

            /*
            A block is never placed at a position: it is placed against the face of a neighbour, and
            the server works out where it lands. The caller's face is which neighbour to try first,
            not the only one.
            */
            let chosen = alive(&bot, |game| {
                let feet = BlockPos::from(game.client.position());
                /* Standing in the space is the one placement a server will never accept. */
                if at == feet || at == feet.up(1) {
                    return Err(Failure::refused(
                        "INSIDE_THE_BOT",
                        "Cannot place a block inside the bot itself",
                    ));
                }
                let state = block_at(game, at);
                if !state.is_air() {
                    return Ok(Err(format!("{} already holds {}.", written(at), name(state))));
                }
                std::iter::once(preferred)
                    .chain(FACES.into_iter().filter(|face| *face != preferred))
                    .find(|face| !block_at(game, at.offset_with_direction(*face)).is_air())
                    .map(Ok)
                    .ok_or_else(|| {
                        Failure::refused(
                            "NOTHING_TO_PLACE_AGAINST",
                            format!("No solid block next to {} to place against", written(at)),
                        )
                    })
            })??;
            let chosen = match chosen {
                Ok(chosen) => chosen,
                Err(already) => return Ok(Answer::text(already)),
            };
            let against = at.offset_with_direction(chosen);

            approach(&bot, against).await?;

            alive(&bot, |game| {
                let client = &game.client;
                if client.component::<Inventory>().held_item().is_empty() {
                    return Err(Failure::refused("EMPTY_HAND", "Nothing is in hand to place"));
                }
                /* The neighbour's face that looks back at the empty space, and its middle. */
                let hit = chosen.opposite();
                use_item_on(client, against, hit, against.center() + hit.normal_vec3() * 0.5);
                Ok(())
            })??;

            Ok(Answer::text(format!(
                "Placed a block at {} against its {} face.",
                written(at),
                face_name(chosen)
            )))
        })
    },
};

pub const ACTIVATE_BLOCK: Tool = Tool {
    name: "activate-block",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;

            /*
            What opens a window is this followed by wait-for-window. Nothing is waited for here,
            because a lever has no window and waiting for one would make every button cost a timeout.
            */
            let loaded = alive(&bot, |game| game.client.world().read().get_block_state(at).is_some())?;
            if !loaded {
                return Err(Failure::refused(
                    "NOT_LOADED",
                    format!(
                        "{} is outside the loaded chunks, so there is no block to activate",
                        written(at)
                    ),
                ));
            }

            approach(&bot, at).await?;

            let block = alive(&bot, |game| {
                use_item_on(&game.client, at, Direction::Up, at.center());
                super::editors::opened_command_block(game, at);
                name(block_at(game, at))
            })?;
            Ok(Answer::text(format!("Right-clicked {block} at {}.", written(at))))
        })
    },
};

pub const USE_HELD_ITEM: Tool = Tool {
    name: "use-held-item",
    run: |bot, args| {
        Box::pin(async move {
            let offhand = args["offhand"].as_bool().unwrap_or(false);
            let hold = args["holdMs"].as_u64().unwrap_or(0);
            let place = if offhand { "off-hand" } else { "main hand" };

            let (client, held) = alive(&bot, |game| (game.client.clone(), use_item(&game.client, offhand)))?;

            /*
            A plain use leaves the item in use on purpose -- that is how food is eaten. A hold is what
            a bow needs: the draw builds while the item stays in use, and releasing is what fires it.
            */
            if hold == 0 {
                return Ok(Answer::text(format!("Used {held} in the {place}.")));
            }
            let using = Using(client);
            for _ in 0..hold.div_ceil(TICK_MS) {
                alive(&bot, |_| ())?;
                tick(&bot).await;
            }
            drop(using);
            Ok(Answer::text(format!(
                "Used {held} in the {place}, held for {hold}ms and released."
            )))
        })
    },
};

/// A right-click at the air with the item in a hand, whatever the bot is looking at: activate-block
/// is the click on a block, and this is the one for the item itself. Answers with what was in the
/// hand as the use began, in the words the tools use for it.
pub(super) fn use_item(client: &Client, offhand: bool) -> String {
    let held = describe(client, offhand);
    let hand = if offhand {
        InteractionHand::OffHand
    } else {
        InteractionHand::MainHand
    };
    let seq = client.query_self::<&mut BlockStatePredictionHandler, _>(|mut prediction| prediction.start_predicting());
    let look = *client.component::<LookDirection>();
    client.write_packet(ServerboundUseItem {
        hand,
        seq,
        y_rot: look.y_rot(),
        x_rot: look.x_rot(),
    });
    held
}

/// A held use, released when the call ends however it ends. Left in use, the server goes on
/// treating the bot as drawing a bow or raising a shield for as long as it stays in the world.
/// Released whether or not the server had the item in use: the flag it syncs comes a tick after
/// the use began, a release for nothing is a no-op there, and a press's use key lets go the same way.
pub(super) struct Using(pub(super) Client);

impl Drop for Using {
    fn drop(&mut self) {
        if self.0.ecs.read().get_entity(self.0.entity).is_err() {
            return;
        }
        self.0.write_packet(ServerboundPlayerAction {
            action: Action::ReleaseUseItem,
            pos: BlockPos::default(),
            direction: Direction::Down,
            seq: 0,
        });
    }
}

/// Walk until a block is within reach, feet to its corner.
pub(super) async fn approach(bot: &Bot, block: BlockPos) -> Result<(), Failure> {
    let mut approach = Approach::new();
    let corner = Vec3::new(block.x as f64, block.y as f64, block.z as f64);
    while !approach.reached(bot, corner, &written(block))? {
        tick(bot).await;
    }
    Ok(())
}

/// A right-click on a block with the hit given rather than taken from where the bot looks.
///
/// azalea's own right-click makes one up when the bot is not looking at the block -- its middle,
/// on the top face -- and a block placed against a side then lands on top instead. The bot still
/// turns to the spot, because a server that watches where a player looks should see it look there.
pub(super) fn use_item_on(client: &Client, block: BlockPos, direction: Direction, location: Vec3) {
    client.look_at(location);
    let seq = client.query_self::<&mut BlockStatePredictionHandler, _>(|mut prediction| prediction.start_predicting());
    client.write_packet(ServerboundUseItemOn {
        hand: InteractionHand::MainHand,
        block_hit: BlockHit {
            block_pos: block,
            direction,
            location,
            inside: false,
            world_border: false,
        },
        seq,
    });
    client.write_packet(ServerboundSwing {
        hand: InteractionHand::MainHand,
    });
}

/// The block at a position, and air outside the loaded chunks, which is what the game's own client
/// reads there.
fn block_at(game: &Game, at: BlockPos) -> BlockState {
    game.client.world().read().get_block_state(at).unwrap_or_default()
}

/// A block's name without its namespace, the way the other kind of bot writes it in a sentence.
fn name(state: BlockState) -> &'static str {
    plain(BlockKind::from(state).to_str())
}

/// The stack in a hand, in the words the other kind of bot uses: what a server called it, else what
/// it is.
fn describe(client: &Client, offhand: bool) -> String {
    let inventory = client.component::<Inventory>();
    let stack = if offhand {
        &inventory.inventory_menu.as_player().offhand
    } else {
        inventory.held_item()
    };
    if stack.is_empty() {
        return "an empty hand".into();
    }
    let name = match stack.get_component::<CustomName>() {
        Some(custom) => custom.name.to_string(),
        None => stacks::name(stack).to_owned(),
    };
    format!("{name} x{}", stack.count())
}

fn face(name: &str) -> Result<Direction, Failure> {
    FACES
        .into_iter()
        .find(|face| face_name(*face) == name.to_lowercase())
        .ok_or_else(|| Failure::bad_args(format!("unknown face {name}")))
}

fn face_name(face: Direction) -> &'static str {
    match face {
        Direction::Down => "down",
        Direction::Up => "up",
        Direction::North => "north",
        Direction::South => "south",
        Direction::West => "west",
        Direction::East => "east",
    }
}
