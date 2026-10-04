# syntax=docker/dockerfile:1.7

# The toolchain is the one rust-toolchain.toml names. azalea builds on nightly only, and a nightly
# from after its release breaks it, so the date is pinned rather than following the channel.
FROM rust:1-slim-trixie AS build
RUN rustup toolchain install nightly-2026-03-27 --profile minimal
WORKDIR /src
COPY . .
# The target cache is shared by every build on the machine, so it can hold artifacts from another
# branch that are newer than the files COPY just wrote. Cargo trusts mtimes for path crates and would
# then ship that other binary without a word. Touching our own sources makes them the newest thing.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find src vendor -type f -exec touch {} + \
    && cargo build --release && cp target/release/bot-azalea /bot-azalea

FROM debian:trixie-slim
COPY --from=build /bot-azalea /usr/local/bin/bot-azalea
# Nothing the bot does needs root. It writes nothing either, unless it was told to authenticate with
# Microsoft: the credential azalea caches is the one thing it keeps, and /auth is where a volume
# holding it goes. The directory is made here and owned by the bot's own uid because a volume
# mounted on a path the image does not have is created owned by root, and the login would then be
# refused the write with nowhere to say so. 0700 because what is in it is a login.
RUN install -d -m 700 -o 65532 -g 65532 /auth
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/bot-azalea"]
