//! WorldEdit's selection, taken from the channel it draws one on.
//!
//! WorldEdit describes the selection to a client that can draw it over the `worldedit:cui` plugin
//! channel, which is what the WorldEditCUI mod exists to receive. The messages are pipe-separated
//! ASCII: `s|<shape>` names the shape and starts a fresh description, and
//! `p|<0 or 1>|<x>|<y>|<z>|<volume>` gives a corner. Reading them is how a bot learns what it has
//! selected without the server having to say it in chat, which is slow, translated, and off
//! altogether on a server that silences the plugin.
//!
//! Nothing arrives unasked. WorldEdit sends to a session it believes has a CUI client, and a client
//! says so by sending `v|<protocol version>` back on the same channel. It answers a repeated
//! announcement by describing the selection again, which is what turns the handshake into a
//! question: [`ask`] is sent, and the description that comes back is the selection as the server
//! holds it now.

use azalea::protocol::packets::game::ServerboundCustomPayload;
use azalea::{BlockPos, Client, Identifier};

/// The channel both WorldEdit and FastAsyncWorldEdit register, incoming and outgoing.
const NAMESPACE: &str = "worldedit";

const CHANNEL: &str = "cui";

/// Where a client lists the channels it speaks, which is how a vanilla one asks to be sent them.
const REGISTER: &str = "minecraft:register";

/// What the announcement claims to speak. WorldEdit compares it against the selector's own protocol
/// version to choose between the current description and the legacy one; 4 is what WorldEditCUI
/// sends, and FastAsyncWorldEdit refuses an announcement longer than four characters, so this is
/// both current and short enough to be read.
const PROTOCOL_VERSION: u32 = 4;

const SHAPE: &str = "s";

const POINT: &str = "p";

/// A cuboid has two corners, and WorldEdit numbers them 0 and 1.
const CORNERS: usize = 2;

pub fn is_cui(identifier: &Identifier) -> bool {
    identifier.namespace() == NAMESPACE && identifier.path() == CHANNEL
}

/// Say this client draws selections, and let WorldEdit describe the one it holds.
pub fn ask(client: &Client) {
    send(
        client,
        format!("{NAMESPACE}:{CHANNEL}"),
        format!("v|{PROTOCOL_VERSION}"),
    );
}

/// The same, with the channel registration a vanilla client sends first.
///
/// Bukkit puts a plugin's message on the wire whether or not the client registered the channel, so
/// the registration is not what makes WorldEdit answer. It is what a proxy in the middle reads: one
/// that forwards only the channels a client asked for would otherwise drop every description.
pub fn announce(client: &Client) {
    send(client, REGISTER.to_owned(), format!("{NAMESPACE}:{CHANNEL}"));
    ask(client);
}

fn send(client: &Client, channel: String, message: String) {
    client.write_packet(ServerboundCustomPayload {
        identifier: Identifier::new(channel),
        data: message.into_bytes().into(),
    });
}

/// What WorldEdit has said about the selection, as a CUI client keeps it.
#[derive(Default)]
pub struct Selection {
    shape: Option<String>,
    corners: [Option<BlockPos>; CORNERS],
    volume: Option<i64>,
    described: u32,
}

impl Selection {
    /// How many messages about the selection the server has sent.
    ///
    /// A reader takes this before it asks and watches for it to move, which is the only sign that
    /// what it is about to read answers its own question rather than being whatever was left from
    /// the last one.
    pub fn described(&self) -> u32 {
        self.described
    }

    pub fn shape(&self) -> Option<&str> {
        self.shape.as_deref()
    }

    pub fn volume(&self) -> Option<i64> {
        self.volume
    }

    /// The corners the server has named, in its own numbering, with the ones it has not left out.
    pub fn corners(&self) -> impl Iterator<Item = (usize, BlockPos)> + '_ {
        self.corners
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(index, corner)| corner.map(|at| (index, at)))
    }

    /// One message, applied the way a CUI client applies it.
    ///
    /// A shape starts a description over, so the corners from the one before it are dropped:
    /// without that, clearing a selection and reading it back would answer with the corners of the
    /// selection that is gone. A corner then lands in its own slot, since WorldEdit sends only the
    /// corner that changed when a single `//pos1` moves it.
    pub fn accept(&mut self, message: &str) {
        let mut fields = message.split('|');

        match fields.next() {
            Some(SHAPE) => {
                self.shape = fields.next().map(str::to_owned);
                self.corners = [None; CORNERS];
                self.volume = None;
                self.described += 1;
            }
            Some(POINT) => {
                let told: Vec<&str> = fields.collect();
                let [index, x, y, z, rest @ ..] = told.as_slice() else {
                    return;
                };
                let (Ok(index), Ok(x), Ok(y), Ok(z)) = (index.parse::<usize>(), x.parse(), y.parse(), z.parse()) else {
                    return;
                };
                if index >= CORNERS {
                    return;
                }
                self.corners[index] = Some(BlockPos::new(x, y, z));
                self.volume = rest.first().and_then(|volume| volume.parse().ok());
                self.described += 1;
            }
            /*
            Everything else a CUI client draws -- the colours, the ellipsoid and cylinder
            descriptions, the multi-region events -- says nothing about where a cuboid's corners
            are, and a bot has no selection box to paint.
            */
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The messages are WorldEdit's own, in the form `SelectionPointEvent` and the Bukkit player
    /// that sends it produce: the type id, then the parameters, joined with pipes.
    #[test]
    fn a_corner_is_read_where_the_server_put_it() {
        let mut selection = Selection::default();

        selection.accept("s|cuboid");
        selection.accept("p|0|12|64|-30|1");
        selection.accept("p|1|14|66|-28|27");

        assert_eq!(selection.shape(), Some("cuboid"));
        assert_eq!(selection.volume(), Some(27));
        assert_eq!(
            selection.corners().collect::<Vec<_>>(),
            vec![(0, BlockPos::new(12, 64, -30)), (1, BlockPos::new(14, 66, -28))]
        );
    }

    /// A shape starts a description over. Without that, a selection that was cleared and described
    /// again would still answer with the corners of the one that is gone, which is the difference
    /// between "nothing is selected" and putting a box down in the wrong place.
    #[test]
    fn a_shape_drops_the_corners_before_it() {
        let mut selection = Selection::default();

        selection.accept("s|cuboid");
        selection.accept("p|0|12|64|-30|1");
        selection.accept("p|1|14|66|-28|27");
        selection.accept("s|cuboid");

        assert_eq!(selection.corners().count(), 0);
        assert_eq!(selection.volume(), None);
    }

    /// `//pos1` on its own moves one corner, and WorldEdit describes only that one.
    #[test]
    fn one_corner_changes_without_the_other() {
        let mut selection = Selection::default();

        selection.accept("s|cuboid");
        selection.accept("p|0|12|64|-30|1");
        selection.accept("p|1|14|66|-28|27");

        let described = selection.described();

        selection.accept("p|0|0|64|0|29767");

        assert_eq!(selection.described(), described + 1);
        assert_eq!(
            selection.corners().collect::<Vec<_>>(),
            vec![(0, BlockPos::new(0, 64, 0)), (1, BlockPos::new(14, 66, -28))]
        );
    }

    /// The channel carries more than this reads -- colours, ellipsoids, the multi-region events --
    /// and a message it cannot make sense of must leave what it already knows alone rather than
    /// half-applying itself.
    #[test]
    fn what_cannot_be_read_changes_nothing() {
        let mut selection = Selection::default();

        selection.accept("s|cuboid");
        selection.accept("p|0|12|64|-30|1");

        let described = selection.described();

        selection.accept("col|0xff0000ff|0x00ff00ff|0x0000ffff|0xffffffff");
        selection.accept("p|0|not-a-number|64|-30|1");
        selection.accept("p|1|3|4");
        selection.accept("");

        assert_eq!(selection.described(), described);
        assert_eq!(
            selection.corners().collect::<Vec<_>>(),
            vec![(0, BlockPos::new(12, 64, -30))]
        );
    }
}
