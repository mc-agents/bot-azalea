# azalea-protocol 0.16.0+mc26.1, patched

As released on crates.io, with three changes.

## `ClientboundCooldown` in `src/packets/game/c_cooldown.rs`

The released struct reads `item: ItemKind`, an item registry id as a VarInt. Since 1.21.2 the game
sends a cooldown group instead (`ClientboundCooldownPacket(Identifier cooldownGroup, int
duration)`), so azalea read the identifier's length as an item and its first letter as the duration,
and every cooldown the server started arrived as nonsense. The field is now
`cooldown_group: Identifier`; nothing in azalea reads the old one.

## `ItemStackSlotDisplay` in `src/common/recipe.rs`

The released struct holds an `ItemStack`, which reads a count and then an item. 26.x sends a
display's stack as an `ItemStackTemplate` -- the item, then the count, then the components -- so the
two fields arrived swapped: a recipe making four sticks read as one polished granite, and every
stonecutter result was named after its count. The field is now an `ItemStackTemplate`; nothing in
azalea reads it.

## `ServerboundPlaceRecipe` in `src/packets/game/s_place_recipe.rs`

The released struct names the recipe by `Identifier`. Since 1.21.2 the game sends the display id
the recipe book gave it (`RecipeDisplayId`, a VarInt), and a server cannot decode the old shape. The
field is now `#[var] recipe: u32`.

Delete this directory and its `[patch.crates-io]` line once azalea's own structs read all three
this way.
