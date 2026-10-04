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
./hack/image.sh                          # bot-azalea:local-mc26.1.2
./hack/image.sh my:tag
python3 hack/sync-catalog.py --from v0.61.3   # pin the catalogue of an mcp-server release
```

One Minecraft version, 26.1.2, because one azalea release speaks one protocol: `0.16.0+mc26.1` is
protocol 775, which is 26.1.2's.

`src/catalog.rs` is generated and names the mcp-server release it came from as `CATALOG_SOURCE`.
CI regenerates it from that release and fails on any difference, so a catalogue change is a
diff to review, never a hash the server quietly stops accepting. CI also runs `cargo fmt --check`,
`cargo clippy -- -D warnings` and `cargo test` on the nightly `rust-toolchain.toml` pins, then
builds the image and runs it against a Paper server (`dev/compose.yml`, `dev/smoke.py`) and
mcp-server's `dev/conform.py`. A push whose changes are outside what the image is built from
neither publishes nor tags; one that ships gets `v<version>` and a GitHub release with the
commits since the previous tag.

```sh
docker compose -f dev/compose.yml up -d              # Paper 26.1.2, offline, flat, port 25578
python3 dev/smoke.py --port 8765 --server host.docker.internal 25578
docker run --rm --add-host host.docker.internal:host-gateway \
    -e MCP_SERVER_HOST=host.docker.internal -e BOT_NAME=azalea_bot bot-azalea:local-mc26.1.2
```

## Running it

It reads everything from the environment. Everything but the three `BOT_AUTH*` is the protocol
document's; those three are this image's own, because which account a bot joins a game server with
is not something the mcp-server it links to has an opinion about.

| variable | meaning | default |
| --- | --- | --- |
| `MCP_SERVER_HOST` | the mcp-server to dial | `127.0.0.1` |
| `MCP_SERVER_PORT` | | `8765` |
| `BOT_NAME` | the name sent in `hello`, and the name an agent addresses | `HOSTNAME`, else `azalea`. A compose replica has no name of its own, so it takes its container's, which is what `--scale bot=N` needs to mean anything |
| `BOT_LINK_TOKEN` | goes into `hello` as `linkToken`, so a server that holds one can tell its own bots from anything else that reached the port. The operator sets it from the server's link Secret; a bot run by hand against a server without one leaves it unset | unset: no `linkToken` in `hello` |
| `HEALTH_PORT` | where `/healthz` and `/readyz` are served | `8080` |
| `RECONNECT_MIN_MS`, `RECONNECT_MAX_MS` | backoff bounds for redialling | `500`, `15000` |
| `BOT_AUTH` | `offline` or `microsoft`. Anything else and the bot says so and stops, rather than guessing | `offline` |
| `BOT_AUTH_ACCOUNT` | `microsoft` only, and required: the account to log in as, which is the key its cached credential is held under. Usually the email | unset |
| `BOT_AUTH_CACHE` | `microsoft` only: the file azalea keeps the credential in. A directory of its own, so the volume that carries a login carries nothing else | `/auth/azalea-auth.json` |

```sh
docker run --rm -e MCP_SERVER_HOST=host.docker.internal -e BOT_NAME=a1 bot-azalea:local-mc26.1.2
```

`python3 ../mcp-server/dev/conform.py 18777` holds it to the bot's half of the protocol.

### Offline, which is the default

The bot sends a username and the game server takes it on trust, which needs `online-mode=false` in
that server's `server.properties`. That is a real cost to whoever runs it, and it is theirs and not
the bot's: with authentication off, anyone who can reach the port can join under any name, including
a name the server has opped, so the server has to be unreachable from the internet or sit behind a
proxy that authenticates in front of it. Offline UUIDs are derived from the name rather than issued
with the account, so homes, permissions and inventories keyed by UUID belong to the name, and a world
moved between the two modes leaves the player data it already had behind.

It is also the only one of the two modes `docker compose up --scale bot=N` means anything in:
replicas share every environment variable, and the only thing that tells them apart is the hostname
each takes its name from.

### Microsoft, for a server that stays in online mode

One Minecraft account is one player, so these are services written out by hand, one account each,
never `--scale`. Log in once, with a terminal attached:

```sh
docker compose run --rm bot-login
# or, without compose:
docker run --rm -it -v azalea-auth:/auth -e BOT_AUTH_ACCOUNT=you@example.com \
    bot-azalea:local-mc26.1.2 login
```

That is the image run with its one argument, `login`: azalea prints a Microsoft link with the code
already in it, waits for somebody to open it, writes the credential into `BOT_AUTH_CACHE` and exits.
The bot mounts the same volume and refreshes from that file at every start. Started with no cache to
read it writes what is wrong and exits non-zero, rather than linking and then failing every join
afterwards, which reads as the game server's fault.

The device flow is what it is kept away from, because inside a long-running bot both halves of it are
lost -- the link goes to a log nobody is attached to, and the wait holds the join that asked for it.
It is kept away from, not made impossible, and two paths still reach it. A cache that exists but
holds nothing under this `BOT_AUTH_ACCOUNT` -- a typo, or a volume logged in to under another
account -- drops the start-up sign-in into it; that one is bounded, and the bot gives up after a
minute and says which of the two it was. The other is not bounded: a refresh token lasts about
ninety days, and a bot still running past that gets `InvalidSession` mid-join, which azalea answers
by refreshing, which with nothing to refresh from starts the device flow in the join's own thread
with no deadline on it. Log in again before that, and if a long-lived bot ever does go quiet on a
join, that is the first thing to look at.

The account decides the name. `join-server` may ask for another one and cannot have it: the bot joins
as the account and says so, in the answer to `connect` and in a `warn` over the link. Set `BOT_NAME`
to the account's Minecraft name and nothing diverges.

One thing has not moved with the mode: `send-chat` still refuses with `CHAT_UNSIGNED` against a
server that enforces secure chat. That refusal reads a flag in the login packet and predates there
being an account that could hold a signing key at all; it has not been retested against one.

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
- **A book screen's buttons are not modelled.** `press-dialog-button` with a chest or a book open
  and no dialog names the screen and offers no buttons, as `bot-fabric` does on a chest; on a book
  or a lectern `bot-fabric` presses "Done" or "Take Book", and this bot refuses with
  `NO_SUCH_BUTTON`. `close-window` closes either, and a lectern's "take book" is
  `press-container-button`'s.
- **An anvil's name box is not modelled.** `type-text` with an anvil open refuses with
  `UNSUPPORTED_INPUT`, where `bot-fabric` types into the box and the server renames the item.
- **Every use is let go of with a release packet.** The client predicts whether a right-click
  started using the item and releases only then; this bot has only the flag the server syncs a
  tick later, so `press: use`, `use-held-item` with a hold and a `useItem` step all send the
  release, which the server ignores when nothing is in use.
- **A disconnected client stays in the ECS.** Joining again and again in one process grows it.

## License

Apache License 2.0; see [LICENSE](LICENSE).
