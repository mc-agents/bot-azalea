# bot-azalea

A headless Minecraft bot for [mc-agents](https://github.com/mc-agents/mcp-server), built on
[azalea](https://github.com/azalea-rs/azalea). It dials `mcp-server`, answers the tools it
implements, and is the kind an agent picks when it wants many bots at once or a join in under a
second -- not a screenshot, a dialog or a book, which are `bot-fabric`'s.

The two kinds speak one protocol (`mcp-server/docs/bot-protocol.md`) and are held to one set of
end-to-end sentences. Which tools this one runs is the catalogue's `kinds`, not this README.

## What it costs

Measured on an M-series Mac under OrbStack (10 vCPU), against Paper 26.1.2, one bot per container,
each pinned to one CPU. Numbers from the spike that preceded the protocol code; they will be
measured again against this binary before they are relied on.

| | |
| --- | --- |
| memory, one bot | 15-20 MiB RSS, about 8 MiB of it the bot's own |
| memory, fifty bots | 396 MiB across the fifty containers |
| process start to spawned | 0.5-0.65 s on average; the slowest of fifty was 5.8 s, which is Paper's connection throttle |
| CPU, idle in a world | about 3% of a core per bot |

For comparison, a `bot-fabric` container is about 1.7 GiB and takes a minute and a half to link.

The CPU figure depends on one setting. azalea's defaults give its ECS a worker per core, and an
idle bot waking twenty-two threads sixty times a second used a quarter to a half of a core; the bot
runs it on one thread instead.

## Building

azalea builds on nightly Rust only, and a nightly newer than the crate breaks it, so the toolchain
is pinned in `rust-toolchain.toml`. Nothing needs installing on the host: the image builds inside
a container.

```sh
./hack/image.sh                 # bot-azalea:local-mc26.1.2
./hack/image.sh my:tag
python3 hack/sync-catalog.py    # after mcp-server's catalogue changes
```

One Minecraft version, 26.1.2, because one azalea release speaks one protocol: `0.16.0+mc26.1` is
protocol 775, which is 26.1.2's.

## Running it

It reads everything from the environment, as the protocol document lists: `MCP_SERVER_HOST`,
`MCP_SERVER_PORT`, `BOT_NAME`, `HEALTH_PORT`, `RECONNECT_MIN_MS`, `RECONNECT_MAX_MS`.

```sh
docker run --rm -e MCP_SERVER_HOST=host.docker.internal -e BOT_NAME=a1 bot-azalea:local-mc26.1.2
```

`python3 ../mcp-server/dev/conform.py 18777` holds it to the bot's half of the protocol.

## What was changed underneath

- **`vendor/azalea-chat`.** A keybind, selector, score, nbt or object component made azalea fail
  the decode of the whole packet, so the chat line it was in never reached the bot and all that was
  left was a log line. Those now stand in as text naming what could not be resolved. A click event
  that shows a dialog keeps the dialog, which azalea dropped. A styled number format's style is
  read as the unnamed NBT the network sends rather than as a named compound, and a team's colour
  with bold and strikethrough in the order the game numbers them, which azalea had swapped. The
  rest of the crate is as released.
- **The attack packet is written by the bot.** azalea writes the target's id as a four-byte int
  where the game reads a VarInt, and the server drops the connection of a bot that swings. So is
  the interact packet: azalea sends the click as a position in the world, where the game sends it
  relative to the entity, and sends it twice.
- **A window is read after the server answers the click.** azalea predicts a click before sending
  it, and a number-key swap or a drag comes out differently from the game's. Every click goes with
  a state id the server never holds, which makes it answer with the whole window, and the tool reads
  that. The state id and cursor azalea drops from those answers are put back by the bot.
- **`vendor/azalea-protocol`.** azalea decodes the cooldown packet as an item where 26.x sends a
  cooldown group, so every cooldown a server started arrived as nonsense, and reads an objective's
  number format without the presence boolean 26.x writes in front of it. A few structs are changed;
  `vendor/azalea-protocol/PATCHED.md` says which and when the directory can go.
- **No automatic respawn or reconnect.** A server under test may be checking what happens on death
  or on a kick, and a bot that got up or rejoined by itself would hide exactly that.

## Known limits

- **Components arrive as azalea parsed them.** Hover events are dropped. A translate key is
  resolved with the en_us table azalea ships, so "Golden Apple" reads as it does in a default
  client; a key only a resource pack defines stays a key. Fonts and colours survive.
- **Much of what a server sends is decoded and not kept by azalea.** The action bar, titles, dialogs,
  boss bars, the scoreboard and the clock are tracked here (`src/hud.rs`); advancements and block
  entities are not yet, and nothing reads them.
- **Commands go unsigned.** azalea signs chat and nothing else, so a server in online mode that
  enforces secure profiles will not run a command with a signed argument, such as `/msg`.
- **A click the server never answers is read as answered.** `click-slot` and a `click` step of
  `run-inputs` wait two seconds for the window to come back and then read what there is, where
  `bot-fabric` stops with `CLICK_UNCONFIRMED`. A `press` step ends up to three ticks after the key
  came up, once azalea has told the server, and `endedTick` reports that.
- **A disconnected client stays in the ECS.** Joining again and again in one process grows it.
