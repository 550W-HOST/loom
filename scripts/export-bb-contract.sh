#!/usr/bin/env bash
# Regenerate contracts/bb from a bb checkout.
#
# bb is read-only input: the exporter copies the contract packages into a
# scratch module tree, imports them, and writes JSON Schema artifacts. It never
# writes into the bb checkout and never edits bb's contract definitions.
#
# Usage:
#   BB_SRC=/path/to/bb scripts/export-bb-contract.sh
#   scripts/export-bb-contract.sh /path/to/bb
#
# Requires bun (https://bun.sh). Network access is needed on the first run to
# install the exporter's pinned zod/hono/typescript.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bb_src="${BB_SRC:-${1:-}}"

if [[ -z "$bb_src" ]]; then
  echo "usage: BB_SRC=/path/to/bb $0 [path-to-bb]" >&2
  exit 2
fi
if [[ ! -d "$bb_src/packages/server-contract/src" ]]; then
  echo "error: $bb_src is not a bb checkout (packages/server-contract/src missing)" >&2
  exit 2
fi
if ! command -v bun >/dev/null 2>&1; then
  echo "error: bun is required to run the exporter" >&2
  exit 2
fi

bb_src="$(cd "$bb_src" && pwd)"

cd "$repo_root/tools/contract-export"
bun install --frozen-lockfile
bun run src/index.ts --bb "$bb_src" --out "$repo_root/contracts/bb"
