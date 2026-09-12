#!/usr/bin/env bash
# Verify a set of release binaries, before anyone downloads them.
#
# A release makes three claims that only running the artifact can settle, and
# the third is why this is a file rather than a build step:
#
#   1. the binary runs on the machine it targets at all — a static musl build is
#      where a glibc assumption surfaces, and it surfaces at run time
#   2. `loom-server` answers `/health`, serves its embedded UI from the same
#      origin, and creates a project
#   3. `loom-daemon` enrols against that server, which is the protocol handshake
#      a mismatched pair of artifacts would refuse
#
# The pipeline runs this on the x86_64 artifacts, and a maintainer can run it
# against a downloaded release. The aarch64 artifacts cannot be executed on an
# x86_64 machine, so for those the pipeline runs the same script with
# `--elf-only`: every check a foreign machine can still make — the ELF is the
# target architecture, it is statically linked, and the commit the build stamped
# is in the file.
#
# Usage:
#   scripts/verify-release-binaries.sh <bin-dir> [options]
#
#   <bin-dir>                directory holding `loom-server` and `loom-daemon`
#   --expect-commit SHA      require that commit, the one the tag names
#   --expect-target TRIPLE   require that target triple
#   --elf-only               do not execute the binaries (foreign architecture)
#   -h, --help               this text
#
# Needs curl, jq, file, readelf, and coreutils (`mktemp`, `shuf`, `seq`).
# Nothing here reads the source tree: the binaries are asked, never the
# checkout.

set -euo pipefail

usage() { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }

bin_dir=""
expect_commit=""
expect_target=""
elf_only=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --elf-only)
      elf_only=1
      shift
      ;;
    --expect-commit)
      expect_commit="${2:-}"
      shift 2
      ;;
    --expect-target)
      expect_target="${2:-}"
      shift 2
      ;;
    -*)
      die "unrecognised option: $1"
      ;;
    *)
      [[ -z "$bin_dir" ]] || die "unexpected argument: $1"
      bin_dir="$1"
      shift
      ;;
  esac
done

[[ -n "$bin_dir" ]] || {
  usage >&2
  exit 2
}

server="$bin_dir/loom-server"
daemon="$bin_dir/loom-daemon"
[[ -x "$server" ]] || die "$server is missing or not executable"
[[ -x "$daemon" ]] || die "$daemon is missing or not executable"
tools=(file readelf)
[[ "$elf_only" -eq 1 ]] || tools+=(curl jq)
for tool in "${tools[@]}"; do
  command -v "$tool" >/dev/null || die "$tool is required"
done

####################################################################
# The file itself
####################################################################

# `file` and `readelf` read a binary of any architecture, so these are the
# checks that survive cross-compilation. A dynamic aarch64 binary is the failure
# that matters: it looks fine on the build machine and cannot start on a
# musl-only host.
elf_arch=""
case "$expect_target" in
  x86_64*) elf_arch="x86-64" ;;
  aarch64*) elf_arch="ARM aarch64" ;;
  "") ;;
  *) die "no ELF machine name is known for $expect_target" ;;
esac

check_elf() {
  local binary="$1" description
  description="$(file -b "$binary")"
  note "$(basename "$binary"): $description"
  case "$description" in
    *ELF\ 64-bit*) ;;
    *) die "$binary is not a 64-bit ELF: $description" ;;
  esac
  if [[ -n "$elf_arch" ]]; then
    case "$description" in
      *"$elf_arch"*) ;;
      *) die "$binary is not $elf_arch: $description" ;;
    esac
  fi
  # Self-contained means what an operator needs it to mean: no loader to find
  # and no shared library to resolve, so the file runs on a host that has
  # neither. Both shapes loom ships satisfy that and they are not the same
  # shape — x86_64 is a static PIE, which carries a dynamic section so it can
  # relocate itself, and aarch64 is a plain static executable — so this tests
  # for the absence of `NEEDED` and `INTERP` rather than the absence of a
  # dynamic section, which a static PIE has. For the same reason `file` calls
  # the static PIE "dynamically linked" and cannot decide this.
  if readelf -d "$binary" | grep -q '(NEEDED)'; then
    die "$binary links against a shared library: $(readelf -d "$binary" | grep '(NEEDED)')"
  fi
  if readelf -l "$binary" | grep -q 'INTERP'; then
    die "$binary has an interpreter, so it needs the host's dynamic loader"
  fi
}

check_elf "$server"
check_elf "$daemon"

if [[ "$elf_only" -eq 1 ]]; then
  # No `--version` here: the binary cannot be executed on this machine. The
  # stamp is still checkable as bytes, which is what catches a build whose stamp
  # never landed — a failure that would otherwise reach a user as `commit
  # unknown`.
  if [[ -n "$expect_commit" ]]; then
    for binary in "$server" "$daemon"; do
      grep -q -- "$expect_commit" "$binary" || die "$binary does not carry commit $expect_commit"
    done
    note "commit $expect_commit is stamped in both binaries"
  fi
  printf 'checked %s (not executed: foreign architecture)\n' "$bin_dir"
  exit 0
fi

####################################################################
# What the binaries say about themselves
####################################################################

# `<name> <version> (<target>, protocol <n>, commit <sha>)`. Parsed rather than
# matched loosely, because a release that cannot name its commit is the defect
# this whole section exists to catch, and a parse that yielded empty fields
# silently would make every comparison below pass.
read_version() {
  local name="$1" binary="$2" line parsed
  line="$("$binary" --version)" || die "$binary --version failed"
  parsed="$(printf '%s\n' "$line" |
    sed -E "s/^$name ([^ ]+) \(([^,]+), protocol ([0-9]+), commit ([^)]+)\)$/\1\t\2\t\3\t\4/")"
  # An unmatched line passes through unchanged, which is the shape check.
  [[ "$parsed" != "$line" ]] || die "$binary --version printed an unexpected shape: $line"
  IFS=$'\t' read -r VERSION TARGET PROTOCOL COMMIT <<<"$parsed"
  note "$line"
  [[ "$COMMIT" != "unknown" ]] ||
    die "$binary carries no commit: it was built without a repository and without LOOM_GIT_COMMIT"
  if [[ -n "$expect_commit" && "$COMMIT" != "$expect_commit" ]]; then
    die "$binary was built from $COMMIT, not $expect_commit"
  fi
  if [[ -n "$expect_target" && "$TARGET" != "$expect_target" ]]; then
    die "$binary was built for $TARGET, not $expect_target"
  fi
}

read_version loom-server "$server"
release_version="$VERSION"
release_commit="$COMMIT"
release_target="$TARGET"
release_protocol="$PROTOCOL"

# The two binaries ship as one set and are upgraded together, so a set that
# disagrees with itself is a broken release even though each half looks fine.
read_version loom-daemon "$daemon"
[[ "$VERSION" == "$release_version" ]] ||
  die "loom-server is $release_version but loom-daemon is $VERSION"
[[ "$COMMIT" == "$release_commit" ]] ||
  die "loom-server is built from $release_commit but loom-daemon from $COMMIT"
[[ "$TARGET" == "$release_target" ]] ||
  die "loom-server is built for $release_target but loom-daemon for $TARGET"
[[ "$PROTOCOL" == "$release_protocol" ]] ||
  die "loom-server speaks protocol $release_protocol but loom-daemon speaks $PROTOCOL"

####################################################################
# Running them
####################################################################

tmp="$(mktemp -d)"
server_pid=""
daemon_pid=""

# Both logs are printed on failure: a release check that fails without saying
# why costs more than it saves.
logs() {
  for log in "$tmp/server.log" "$tmp/daemon.log"; do
    [[ -s "$log" ]] || continue
    printf -- '--- %s\n' "$log" >&2
    tail -n 20 "$log" >&2
  done
}

cleanup() {
  [[ -z "$daemon_pid" ]] || kill "$daemon_pid" 2>/dev/null || true
  [[ -z "$server_pid" ]] || kill "$server_pid" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

# A random high port, retried: this checks a binary, not port allocation, and a
# taken port must not be reported as a broken release.
base=""
for _ in 1 2 3; do
  port="$(shuf -i 20000-44000 -n 1)"
  base="http://127.0.0.1:$port"
  # Cleared first: a failed attempt must not be read as the next one's answer.
  rm -f "$tmp/health.json"
  # The durable backend, not the in-process one: a release is deployed as a
  # service with a data directory, so that is the shape to start.
  LOOM_BIND="127.0.0.1:$port" LOOM_DATA_DIR="$tmp/server" LOOM_NODE_ID="release-verification" \
    "$server" >"$tmp/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 100); do
    curl -fsS --max-time 1 "$base/health" >"$tmp/health.json" 2>/dev/null && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.2
  done
  [[ -s "$tmp/health.json" ]] && break
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
  server_pid=""
done
if [[ -z "$server_pid" || ! -s "$tmp/health.json" ]]; then
  logs
  die "loom-server never answered /health on $base"
fi

health_status="$(jq -r '.status' "$tmp/health.json")"
health_protocol="$(jq -r '.protocol_version' "$tmp/health.json")"
[[ "$health_status" == "ok" ]] || die "/health reported status '$health_status'"
[[ "$health_protocol" == "$release_protocol" ]] ||
  die "/health reports protocol $health_protocol, but --version reports $release_protocol"
note "/health ok (protocol $health_protocol, node $(jq -r .node_id "$tmp/health.json"))"

# The UI is embedded at compile time and served from the API's origin. `/` is a
# client route, so a 200 here also proves the SPA fallback survived the release
# profile, and the asset bodies prove the bundle reached the binary rather than
# being an empty placeholder.
curl -fsS -o "$tmp/index.html" -w '%{http_code}' "$base/" >"$tmp/index.code" ||
  die "GET / failed"
[[ "$(cat "$tmp/index.code")" == "200" ]] || die "GET / did not answer 200"
grep -q 'src="/app.js"' "$tmp/index.html" || die "the served index.html does not reference /app.js"
note "GET / -> 200 text/html, $(wc -c <"$tmp/index.html" | tr -d '[:space:]') bytes"

asset() {
  local path="$1" expected_type="$2" result code type size
  result="$(curl -fsS -o "$tmp/asset" -w '%{http_code}\t%{content_type}\t%{size_download}' "$base$path")" ||
    die "GET $path failed"
  IFS=$'\t' read -r code type size <<<"$result"
  [[ "$code" == "200" ]] || die "GET $path -> $code"
  [[ "$type" == "$expected_type" ]] || die "GET $path -> content-type $type, expected $expected_type"
  [[ "$size" -gt 0 ]] || die "GET $path served an empty body"
  note "GET $path -> 200 $type, $size bytes"
}

asset "/app.js" "text/javascript; charset=utf-8"
asset "/style.css" "text/css; charset=utf-8"

project_id="$(
  curl -fsS -X POST -H 'content-type: application/json' -d '{"name":"release-verification"}' \
    "$base/api/v1/projects" | jq -r '.project.id // empty'
)" || die "POST /api/v1/projects failed"
[[ -n "$project_id" ]] || die "POST /api/v1/projects returned no project id"
# Read it back: the write echoing an id proves the handler ran, the list proves
# the state it published is what a client sees.
curl -fsS "$base/api/v1/projects" |
  jq -e --arg id "$project_id" '.projects[] | select(.id == $id)' >/dev/null ||
  die "project $project_id was created but is not in the project list"
note "created project $project_id and read it back from the list"

# The daemon and the server refuse to work together unless their protocol
# versions match, so an enrolled host is that handshake succeeding on real
# sockets.
"$daemon" --server-url "$base" --name "release-verification" \
  --state "$tmp/host-id" --session-dir "$tmp/sessions" >"$tmp/daemon.log" 2>&1 &
daemon_pid=$!
enrolled=""
for _ in $(seq 1 150); do
  curl -fsS "$base/api/v1/hosts" 2>/dev/null |
    jq -e '.hosts[] | select(.status == "connected")' >/dev/null && {
    enrolled=yes
    break
  }
  kill -0 "$daemon_pid" 2>/dev/null || break
  sleep 0.2
done
if [[ -z "$enrolled" ]]; then
  logs
  die "loom-daemon did not enrol against the server"
fi
host_id="$(curl -fsS "$base/api/v1/hosts" | jq -r '.hosts[0].id')"
note "daemon enrolled as $host_id"

printf 'verified loom %s (%s, protocol %s, commit %s)\n' \
  "$release_version" "$release_target" "$release_protocol" "$release_commit"
