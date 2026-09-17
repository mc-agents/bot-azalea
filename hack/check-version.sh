#!/usr/bin/env bash
# With a base revision given, the version in Cargo.toml must have gone up -- but only when something
# that ends up in the image changed, because the published tag is built from it. A README fixed
# without a release is not a release.
#
# Under GitHub Actions it also writes shipped=true|false, so the workflow can leave the image, the
# tag and the release alone on a push that changed nothing they are built from. With no base there
# is nothing to compare, and publishing is the safe side.
set -o errexit -o nounset -o pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
	echo "$*" >&2
	exit 1
}

mark_shipped() {
	if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
		echo "shipped=$1" >> "${GITHUB_OUTPUT}"
	fi
}

version_at() { sed -n 's/^version = "\(.*\)"/\1/p' | head -1; }

version="$(version_at < "${ROOT}/Cargo.toml")"
[[ -n "${version}" ]] || fail "Cargo.toml has no version"

base="${1:-}"
if [[ -z "${base}" ]]; then
	echo "version ${version}"
	mark_shipped true
	exit 0
fi

RELEASE_PATHS=(src/ vendor/ Cargo.toml Cargo.lock rust-toolchain.toml Dockerfile)

changed="$(git -C "${ROOT}" diff --name-only "${base}" HEAD || true)"
shipped="$(printf '%s\n' "${changed}" | grep -E "^($(IFS='|'; echo "${RELEASE_PATHS[*]}"))" || true)"

if [[ -z "${shipped}" ]]; then
	echo "version ${version}; nothing that ships changed"
	mark_shipped false
	exit 0
fi

previous="$(git -C "${ROOT}" show "${base}:Cargo.toml" 2>/dev/null | version_at || true)"
if [[ -z "${previous}" ]]; then
	echo "version ${version}; ${base} has no Cargo.toml to compare against"
	mark_shipped true
	exit 0
fi

[[ "${previous}" != "${version}" ]] || fail "version is still ${version}; raise it"
highest="$(printf '%s\n%s\n' "${previous}" "${version}" | sort -V | tail -1)"
[[ "${highest}" == "${version}" ]] || fail "version went down: ${previous} -> ${version}"

echo "version ${previous} -> ${version}"
mark_shipped true
