# azalea-protocol 0.16.0+mc26.1, patched

As released on crates.io, with one change: `ClientboundCooldown` in
`src/packets/game/c_cooldown.rs`.

The released struct reads `item: ItemKind`, an item registry id as a VarInt. Since 1.21.2 the game
sends a cooldown group instead (`ClientboundCooldownPacket(Identifier cooldownGroup, int
duration)`), so azalea read the identifier's length as an item and its first letter as the duration,
and every cooldown the server started arrived as nonsense. The field is now
`cooldown_group: Identifier`; nothing in azalea reads the old one.

Delete this directory and its `[patch.crates-io]` line once azalea's own struct takes an identifier.
