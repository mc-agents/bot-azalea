use std::collections::HashMap;
use std::ops::Range;
use std::str::FromStr;

use azalea::BlockPos;
use azalea::block::{BlockState, BlockTrait};
use azalea::registry::Registry;
use azalea::registry::builtin::BlockKind;
use azalea::registry::tags;
use serde_json::{Value, json};

use super::args::{boolean, corner, plain, point, position, text, written};
use super::{Tool, in_world};
use crate::calls::{Answer, Failure};

/// The whole state, as /setblock and //set take one: `oak_stairs[facing=north,half=bottom,...]`,
/// the properties in name order, which is the order the game itself writes them in. The kind alone
/// lost every orientation, and on a server whose custom blocks are note blocks in disguise it lost
/// which custom block a note block stood for. Without the minecraft namespace, as every tool here
/// names a block; there is no other namespace to keep, since BlockKind is the vanilla registry.
fn state_name(state: BlockState) -> String {
    let block = Box::<dyn BlockTrait>::from(state);
    let mut properties: Vec<(&str, &str)> = block.property_map().into_iter().collect();
    properties.sort_unstable();

    let kind = plain(BlockKind::from(state).to_str()).to_string();
    if properties.is_empty() {
        return kind;
    }
    let listed: Vec<String> = properties
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    format!("{kind}[{}]", listed.join(","))
}

/// Whether the game counts a block as air.
///
/// The whole air tag, not `BlockState::is_air`, which is the plain kind alone: a cave is cave_air
/// throughout and the End's emptiness is void_air. The other kind of bot reads vanilla's isAir,
/// true for all three, so anything here that asked `is_air` would answer underground what it would
/// not answer in the open, and answer it differently from that bot.
pub(super) fn is_air(state: BlockState) -> bool {
    tags::blocks::AIR.contains(&BlockKind::from(state))
}

pub const GET_BLOCK_INFO: Tool = Tool {
    name: "get-block-info",
    run: |bot, args| {
        Box::pin(async move {
            let at = position(&args)?;
            let state = in_world(&bot, |game| game.client.world().read().get_block_state(at))?;

            /*
            Outside the loaded chunks there is nothing to say about the block, which is not air. The
            name goes without its namespace, as the other kind of bot sends it: the server words the
            DTO as it arrives, and "minecraft:dirt" from one kind is a second answer to one block.
            */
            let block = state.map(|state| {
                let kind = BlockKind::from(state);
                json!({"name": plain(kind.to_str()), "type": kind.to_u32(), "position": point(at)})
            });
            Ok(Answer::data(
                format!("block at {}, {}, {}", at.x, at.y, at.z),
                json!({"position": point(at), "block": block}),
            ))
        })
    },
};

pub const FIND_BLOCKS: Tool = Tool {
    name: "find-blocks",
    run: |bot, args| {
        Box::pin(async move {
            let asked = text(&args, "blockType")?.to_owned();
            let max_distance = args["maxDistance"].as_f64().unwrap_or(16.0);
            let count = args["count"].as_u64().unwrap_or(1) as usize;

            /*
            An unknown name is refused. Looking it up and taking the registry's default -- which is
            air -- answers a typo with every empty block in range.
            */
            let wanted = BlockKind::from_str(plain(&asked))
                .map_err(|_| Failure::refused("NO_SUCH_BLOCK", format!("there is no block called {asked}")))?;

            let found = in_world(&bot, |game| {
                let from = BlockPos::from(game.client.position());
                let world = game.client.world();
                let world = world.read();
                let radius = max_distance.ceil() as i32;
                let limit = max_distance * max_distance;
                let mut found = Vec::new();

                for x in -radius..=radius {
                    for y in -radius..=radius {
                        for z in -radius..=radius {
                            if (x * x + y * y + z * z) as f64 > limit {
                                continue;
                            }
                            let at = BlockPos::new(from.x + x, from.y + y, from.z + z);
                            if world
                                .get_block_state(at)
                                .is_some_and(|state| BlockKind::from(state) == wanted)
                            {
                                found.push(at);
                            }
                        }
                    }
                }

                /* Nearest first, and a fixed order among blocks at the same distance so two runs agree. */
                let distance = |at: &BlockPos| {
                    let (dx, dy, dz) = (at.x - from.x, at.y - from.y, at.z - from.z);
                    dx * dx + dy * dy + dz * dz
                };
                found.sort_by_key(|at| (distance(at), at.x, at.y, at.z));
                found.truncate(count);
                found
            })?;

            Ok(Answer::data(
                format!("found {} {asked}", found.len()),
                json!({
                    "blockType": asked,
                    "maxDistance": max_distance,
                    "positions": found.into_iter().map(point).collect::<Vec<_>>(),
                }),
            ))
        })
    },
};

pub const READ_REGION: Tool = Tool {
    name: "read-region",
    run: |bot, args| {
        Box::pin(async move {
            let (bounds, include_air) = asked(&args)?;

            let region = in_world(&bot, |game| {
                let world = game.client.world();
                let world = world.read();
                let built = world.chunks.min_y()..world.chunks.min_y() + world.chunks.height() as i32;
                Region::read(&bounds, &built, include_air, |at| world.get_block_state(at))
            })?;

            Ok(answer(&bounds, region))
        })
    },
};

/// The box to read and whether to keep the air in it, as a call words them.
fn asked(args: &Value) -> Result<(Bounds, bool), Failure> {
    let bounds = Bounds::between(corner(args, "from")?, corner(args, "to")?);
    Ok((bounds, boolean(args, "includeAir", true)?))
}

/// The region as mcp-server's renderer takes it. Every name below is one the renderer maps by
/// hand, so a key worded differently here is not a field it misses but a region it draws nothing
/// of at all.
fn answer(bounds: &Bounds, region: Region) -> Answer {
    let (width, height, depth) = bounds.size();
    let runs = region
        .runs
        .into_iter()
        .map(|run| json!({"block": run.block, "count": run.count}))
        .collect::<Vec<_>>();

    Answer::data(
        format!("region {} to {}", written(bounds.low), written(bounds.high)),
        json!({
            "from": point(bounds.low),
            "to": point(bounds.high),
            "size": {"x": width, "y": height, "z": depth},
            "blocks": i64::from(width) * i64::from(height) * i64::from(depth),
            "palette": region.palette,
            "runs": runs,
            "missing": region.missing,
            "outside": region.outside,
        }),
    )
}

/// The box a caller asked for, with its corners put in order.
struct Bounds {
    low: BlockPos,
    high: BlockPos,
}

impl Bounds {
    /// Either pair of opposite corners describes the same box, and each axis is settled on its own:
    /// a caller that named the far corner first, or mixed the two, still gets the box it meant.
    fn between(one: BlockPos, other: BlockPos) -> Self {
        Self {
            low: BlockPos::new(one.x.min(other.x), one.y.min(other.y), one.z.min(other.z)),
            high: BlockPos::new(one.x.max(other.x), one.y.max(other.y), one.z.max(other.z)),
        }
    }

    /// How far the box spans on each axis, both corners counting.
    fn size(&self) -> (i32, i32, i32) {
        (
            self.high.x - self.low.x + 1,
            self.high.y - self.low.y + 1,
            self.high.z - self.low.z + 1,
        )
    }
}

struct Run {
    block: usize,
    count: u64,
}

/// A region as it is read: the blocks in order, each one either lengthening the run the one before
/// it opened or starting another.
#[derive(Default)]
struct Region {
    palette: Vec<String>,
    runs: Vec<Run>,
    missing: u64,
    outside: u64,
    /// Where each state seen so far sits in the palette, so a block is named once and not once a position.
    seen: HashMap<BlockState, usize>,
    /// What the open run is made of, or nothing when the position before this one closed it.
    open: Option<BlockState>,
}

impl Region {
    /// Every block in the box, read y, then z, then x, all ascending. The order is part of the
    /// answer -- nothing else says where a run sits -- and it is the order the other kind of bot
    /// walks, so one box read by either is the same runs.
    fn read(
        bounds: &Bounds,
        built: &Range<i32>,
        include_air: bool,
        block: impl Fn(BlockPos) -> Option<BlockState>,
    ) -> Self {
        let (width, _, depth) = bounds.size();
        let mut region = Self::default();

        for y in bounds.low.y..=bounds.high.y {
            /*
            Past the world's floor or ceiling there is no block and never will be, which the caller
            answers differently from a chunk it was not sent. A whole layer is one or the other, so
            the height is asked once for it rather than once a block.
            */
            if !built.contains(&y) {
                region.beyond(width as u64 * depth as u64);
                continue;
            }

            for z in bounds.low.z..=bounds.high.z {
                for x in bounds.low.x..=bounds.high.x {
                    match block(BlockPos::new(x, y, z)) {
                        None => region.unloaded(),
                        Some(state) if !include_air && is_air(state) => region.dropped(),
                        /*
                        Without the minecraft namespace, as get-block-info sends a name. There is no
                        other namespace to keep: BlockKind is the vanilla registry, so a block the
                        other kind of bot would name create:cogwheel arrives here as whatever
                        vanilla block the server sent in its place.
                        */
                        Some(state) => region.push(state),
                    }
                }
            }
        }
        region
    }

    fn push(&mut self, state: BlockState) {
        match self.runs.last_mut() {
            Some(run) if self.open == Some(state) => run.count += 1,
            _ => {
                let index = match self.seen.get(&state) {
                    Some(index) => *index,
                    None => {
                        self.palette.push(state_name(state));
                        self.seen.insert(state, self.palette.len() - 1);
                        self.palette.len() - 1
                    }
                };
                self.runs.push(Run { block: index, count: 1 });
                self.open = Some(state);
            }
        }
    }

    /// A position the client holds no chunk for: counted, and in no run, because there is nothing
    /// to name it with.
    fn unloaded(&mut self) {
        self.missing += 1;
        self.dropped();
    }

    /// The positions of a layer past the world's build limits, counted apart from the ones with no
    /// chunk: one is answered by loading them, the other never resolves.
    fn beyond(&mut self, blocks: u64) {
        self.outside += blocks;
        self.dropped();
    }

    /// A position whose block is left out. It closes the run: what is not reported never joins the
    /// blocks either side of it into one.
    fn dropped(&mut self) {
        self.open = None;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use azalea::BlockPos;
    use azalea::block::{BlockState, BlockTrait};
    use azalea::registry::builtin::BlockKind;
    use serde_json::{Value, json};

    use super::{Bounds, Region, answer, asked};

    /// The three blocks the game calls air, which the other kind of bot drops as one: a cave is
    /// cave_air throughout and the End's emptiness is void_air, so a bot that dropped only the
    /// plain kind would answer the same includeAir with a different build.
    #[test]
    fn leaving_air_out_leaves_out_every_block_the_game_calls_air() {
        let row = row(&[
            BlockKind::Stone,
            BlockKind::Air,
            BlockKind::CaveAir,
            BlockKind::VoidAir,
            BlockKind::Stone,
        ]);
        let bounds = Bounds::between(BlockPos::new(0, 0, 0), BlockPos::new(4, 0, 0));

        let kept = read(&bounds, true, &row);
        assert_eq!(kept.palette, ["stone", "air", "cave_air", "void_air"]);
        assert_eq!(runs(&kept), [(0, 1), (1, 1), (2, 1), (3, 1), (0, 1)]);

        /* And the stone either side stays two runs: joining them would claim a solid span. */
        let left_out = read(&bounds, false, &row);
        assert_eq!(left_out.palette, ["stone"]);
        assert_eq!(runs(&left_out), [(0, 1), (0, 1)]);
    }

    /// A palette entry is written the way every other tool here writes a block, the minecraft
    /// namespace left off: "minecraft:stone" from one kind of bot and "stone" from the other are
    /// two answers to one block. A block that comes back takes the entry it had, a second entry
    /// for it being a second kind of block to whatever reads the runs.
    #[test]
    fn the_palette_names_a_block_without_its_namespace_and_holds_it_once() {
        let row = row(&[BlockKind::Stone, BlockKind::Dirt, BlockKind::Dirt, BlockKind::Stone]);
        let region = read(
            &Bounds::between(BlockPos::new(0, 0, 0), BlockPos::new(3, 0, 0)),
            true,
            &row,
        );

        assert_eq!(region.palette, ["stone", "dirt"]);
        assert_eq!(runs(&region), [(0, 1), (1, 2), (0, 1)]);
    }

    /// A palette entry is the whole state, spelled the way /setblock takes one and the way the
    /// game writes one -- the properties in name order -- so the other kind of bot spells the same
    /// stairs the same way, and a block with no properties is its bare kind.
    #[test]
    fn the_palette_spells_a_block_state_out_in_property_order() {
        let stairs = BlockState::from(azalea::block::blocks::OakStairs {
            facing: azalea::block::properties::FacingCardinal::East,
            half: azalea::block::properties::TopBottom::Top,
            shape: azalea::block::properties::StairShape::Straight,
            waterlogged: azalea::block::properties::Waterlogged(false),
        });
        let held: HashMap<BlockPos, BlockState> = HashMap::from([
            (BlockPos::new(0, 0, 0), stairs),
            (BlockPos::new(1, 0, 0), BlockState::from(BlockKind::Stone)),
        ]);
        let region = Region::read(
            &Bounds::between(BlockPos::new(0, 0, 0), BlockPos::new(1, 0, 0)),
            &(-64..320),
            true,
            move |at| held.get(&at).copied(),
        );

        assert_eq!(
            region.palette,
            [
                "oak_stairs[facing=east,half=top,shape=straight,waterlogged=false]",
                "stone"
            ]
        );
    }

    /// Nothing else says where a run sits, so the walk is the answer's spine, and the other kind of
    /// bot walks it the same way. A layer stays contiguous: a run follows x, and z moves on only
    /// once a row is done.
    #[test]
    fn the_walk_goes_y_then_z_then_x() {
        let held = [
            (BlockPos::new(0, 0, 0), BlockKind::Stone),
            (BlockPos::new(1, 0, 0), BlockKind::Dirt),
            (BlockPos::new(0, 0, 1), BlockKind::Gravel),
            (BlockPos::new(1, 0, 1), BlockKind::Sand),
            (BlockPos::new(0, 1, 0), BlockKind::Clay),
            (BlockPos::new(1, 1, 0), BlockKind::Ice),
            (BlockPos::new(0, 1, 1), BlockKind::Tuff),
            (BlockPos::new(1, 1, 1), BlockKind::Calcite),
        ];
        let region = read(
            &Bounds::between(BlockPos::new(0, 0, 0), BlockPos::new(1, 1, 1)),
            true,
            &held,
        );

        let in_walk_order = ["stone", "dirt", "gravel", "sand", "clay", "ice", "tuff", "calcite"];
        assert_eq!(region.palette, in_walk_order);
    }

    /// Either pair of opposite corners describes the same box, and a caller is as likely to name
    /// one as the other. The answer names the lower corner, and the runs come back in the walk's
    /// order rather than the order the corners were given in.
    #[test]
    fn either_pair_of_opposite_corners_reads_the_same_box() {
        let mixed = Bounds::between(BlockPos::new(5, -1, 9), BlockPos::new(2, 4, 3));
        assert_eq!(
            (mixed.low, mixed.high),
            (BlockPos::new(2, -1, 3), BlockPos::new(5, 4, 9))
        );
        assert_eq!(mixed.size(), (4, 6, 7));

        let held = [
            (BlockPos::new(0, 0, 0), BlockKind::Stone),
            (BlockPos::new(1, 0, 0), BlockKind::Dirt),
        ];
        let (low, high) = (BlockPos::new(0, 0, 0), BlockPos::new(1, 0, 0));
        let forwards = read(&Bounds::between(low, high), true, &held);
        let backwards = read(&Bounds::between(high, low), true, &held);

        assert_eq!(forwards.palette, ["stone", "dirt"]);
        assert_eq!(runs(&backwards), runs(&forwards));
    }

    /// A position inside the world whose chunk the client was never sent is missing; a layer past
    /// the world's floor or ceiling is outside. The answer to one is to fly closer and the answer
    /// to the other is to ask for a box inside the world, so nothing should add them together.
    /// Both end the run they fall in: what is not reported never joins the blocks either side.
    #[test]
    fn a_chunk_that_was_never_sent_is_counted_apart_from_a_layer_out_of_the_world() {
        let held = [
            (BlockPos::new(0, 319, 0), BlockKind::Stone),
            (BlockPos::new(1, 319, 1), BlockKind::Stone),
        ];
        let region = read(
            &Bounds::between(BlockPos::new(0, 319, 0), BlockPos::new(1, 321, 1)),
            true,
            &held,
        );

        /* The two layers above the ceiling are counted whole, four blocks each, not one apiece. */
        assert_eq!((region.missing, region.outside), (2, 8));
        assert_eq!(region.palette, ["stone"]);
        assert_eq!(runs(&region), [(0, 1), (0, 1)]);
    }

    /// mcp-server's renderer picks these names out of the DTO by hand, so one worded differently
    /// here is not a field it does without: it is a region it draws nothing of. The two counts are
    /// given different values on purpose -- swapping the pair of keys is the way to get them wrong
    /// that no other test would see.
    #[test]
    fn the_dto_is_worded_the_way_the_renderer_reads_it() {
        /* A layer in the world and the one above its ceiling, with a corner in no chunk. */
        let bounds = Bounds::between(BlockPos::new(0, 319, 0), BlockPos::new(1, 320, 1));
        let region = read(
            &bounds,
            true,
            &[
                (BlockPos::new(0, 319, 0), BlockKind::Stone),
                (BlockPos::new(1, 319, 0), BlockKind::Stone),
                (BlockPos::new(0, 319, 1), BlockKind::Dirt),
            ],
        );

        let answered = answer(&bounds, region);
        assert_eq!(answered.text, "region (0, 319, 0) to (1, 320, 1)");
        assert_eq!(
            answered.data,
            Some(json!({
                "from": {"x": 0, "y": 319, "z": 0},
                "to": {"x": 1, "y": 320, "z": 1},
                "size": {"x": 2, "y": 2, "z": 2},
                "blocks": 8,
                "palette": ["stone", "dirt"],
                "runs": [{"block": 0, "count": 2}, {"block": 1, "count": 1}],
                "missing": 1,
                "outside": 4,
            }))
        );
    }

    /// Air is read unless a call says not to, which is the other kind of bot's default too: a call
    /// that left includeAir out and got a build with the gaps closed up would be reading a
    /// different region from the one the same call reads there.
    #[test]
    fn a_call_that_leaves_include_air_out_reads_the_air_as_well() {
        assert!(air(&json!({"from": at(0.0, 0.0, 0.0), "to": at(1.0, 1.0, 1.0)})));
        assert!(!air(
            &json!({"from": at(0.0, 0.0, 0.0), "to": at(1.0, 1.0, 1.0), "includeAir": false})
        ));
    }

    /// A fraction only reaches a bot through a call made straight to it, the way the end-to-end
    /// suite makes one: mcp-server refuses a fractional corner before either bot sees it. The two
    /// still have to answer that call with the same box, so the fraction is cut towards zero as
    /// gson cuts it -- flooring would push a negative corner a block further out.
    #[test]
    fn a_fractional_corner_is_cut_towards_zero_as_the_other_bot_cuts_it() {
        let (bounds, _) = asked(&json!({"from": at(-12.5, 0.9, -0.5), "to": at(12.5, 1.0, 0.5)}))
            .unwrap_or_else(|failure| panic!("{}", failure.message));

        assert_eq!(bounds.low, BlockPos::new(-12, 0, 0));
        assert_eq!(bounds.high, BlockPos::new(12, 1, 0));
    }

    fn air(args: &Value) -> bool {
        asked(args).unwrap_or_else(|failure| panic!("{}", failure.message)).1
    }

    /// A corner as a call words one, as numbers on the wire: a whole one arrives as 3.0 as often
    /// as 3.
    fn at(x: f64, y: f64, z: f64) -> Value {
        json!({"x": x, "y": y, "z": z})
    }

    /// The box as this client reads it: an overworld's build limits, and no chunk at all anywhere
    /// but the positions given.
    fn read(bounds: &Bounds, include_air: bool, held: &[(BlockPos, BlockKind)]) -> Region {
        let held: HashMap<BlockPos, BlockState> = held.iter().map(|&(at, kind)| (at, BlockState::from(kind))).collect();
        Region::read(bounds, &(-64..320), include_air, move |at| held.get(&at).copied())
    }

    /// A row of blocks along x from the origin, which is the axis a run follows.
    fn row(kinds: &[BlockKind]) -> Vec<(BlockPos, BlockKind)> {
        kinds
            .iter()
            .enumerate()
            .map(|(x, kind)| (BlockPos::new(x as i32, 0, 0), *kind))
            .collect()
    }

    fn runs(region: &Region) -> Vec<(usize, u64)> {
        region.runs.iter().map(|run| (run.block, run.count)).collect()
    }
}
