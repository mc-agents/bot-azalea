#!/usr/bin/env bash
#
# Build the bot image from the working tree, the way CI builds it.
#
#     ./hack/image.sh              # tagged bot-azalea:local-mc26.1.2
#     ./hack/image.sh my:tag       # or a tag of your own
#
# The Minecraft version is azalea's: one per release, and this bot is built against the one its
# pinned crate speaks.
set -euo pipefail

cd "$(dirname "$0")/.."

tag=${1:-bot-azalea:local-mc26.1.2}
docker build -t "${tag}" .
echo "built ${tag}"
