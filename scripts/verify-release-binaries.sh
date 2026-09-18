#!/usr/bin/env bash
# Verify a set of release binaries, before anyone downloads them.
#
# A release makes five claims that only running the artifact can settle, and the
# ones that need a live server are why this is a file rather than a build step:
#
#   1. the binary runs on the machine it targets at all — a static musl build is
#      where a glibc assumption surfaces, and it surfaces at run time
#   2. `loom-daemon` enrols against `loom-server`, which is the protocol
#      handshake a mismatched pair of artifacts would refuse
#   3. the two together accept a contract-shaped write: the server creates a
#      project on the enrolled host and reads it back from the list
#   4. the server serves the product app compiled into it: `/` is the app's index
#      document, the entry script that document names is present, a client route
#      falls back to it, and an unknown `/api` path is a JSON 404 rather than the
#      shell
#   5. a `LOOM_UI_DIR` left over from the commit range that served a bundle
#      from disk is refused rather than ignored: the binary carries the client,
#      so there is no bundle path to configure and a variable naming one is an
#      operator error
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
# Needs curl, jq, file, readelf, and coreutils (`mktemp`, `shuf`, `seq`,
# `timeout`). Nothing here reads the source tree: the binaries are asked, never
# the checkout. The UI is compiled into the server, so it is asked for what a
# deployment gives it — nothing: the server is started with no UI environment
# variable at all.

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
  # service with a data directory, so that is the shape to start. No UI variable
  # is passed, and any the caller's environment happens to carry is removed:
  # the app is in the binary, and a server configured with one is a server that
  # refuses to start (`env -u` below).
  env -u LOOM_UI_DIR -u LOOM_UI_PROXY \
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

# The app is served out of the binary, with no UI variable set: `/` is a client
# route, so a 200 here also proves the SPA fallback works, and the entry asset
# proves the document the server hands out names a script the same binary really
# carries.
served="$(curl -sS -o "$tmp/index.html" -w '%{http_code}\t%{content_type}' "$base/")" ||
  die "GET / failed"
IFS=$'\t' read -r index_code index_type <<<"$served"
[[ "$index_code" == "200" ]] || die "GET / -> $index_code, expected 200"
[[ "$index_type" == "text/html; charset=utf-8" ]] ||
  die "GET / -> content-type $index_type, expected text/html"
entry="$(sed -n 's/.*<script[^>]*src="\(\/assets\/[^"]*\.js\)".*/\1/p' "$tmp/index.html" | head -n 1)"
[[ -n "$entry" ]] ||
  die "the served index.html has no <script src=\"/assets/...js\"> entry point"
note "GET / -> 200 text/html, $(wc -c <"$tmp/index.html" | tr -d '[:space:]') bytes (entry $entry)"

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

# The entry point, named by the document itself: a build's asset names are
# hashed, so the index is the only place that knows them.
asset "$entry" "text/javascript; charset=utf-8"

# A route the client owns but the server has no file for is the SPA's, so a deep
# link works. Compared against the index just fetched rather than merely accepted
# with a 200: a placeholder page answers 200 too.
curl -fsS -o "$tmp/fallback.html" -w '%{http_code}' "$base/deep/link/into/the/app" >"$tmp/fallback.code" ||
  die "GET /deep/link/into/the/app failed"
[[ "$(cat "$tmp/fallback.code")" == "200" ]] ||
  die "an unknown client route answered $(cat "$tmp/fallback.code"), but the SPA fallback is what a deep link needs"
cmp -s "$tmp/index.html" "$tmp/fallback.html" ||
  die "an unknown client route did not fall back to the embedded index.html"
note "GET /deep/link/into/the/app -> 200 index.html"

# ...but an unknown API path must not fall back: a JSON 404 is what tells a
# client it asked for something that does not exist instead of quietly handing it
# the shell with a 200.
api_miss="$(curl -sS -o "$tmp/api-miss.json" -w '%{http_code}\t%{content_type}' \
  "$base/api/v1/definitely-not-a-route")" ||
  die "GET /api/v1/definitely-not-a-route could not be reached"
IFS=$'\t' read -r miss_code miss_type <<<"$api_miss"
[[ "$miss_code" == "404" ]] || die "an unknown API path answered $miss_code, expected 404"
[[ "$miss_type" == "application/json" ]] ||
  die "an unknown API path answered content-type $miss_type, expected application/json"
note "GET /api/v1/definitely-not-a-route -> 404 application/json"

# The other half of "the client is in the binary": a `LOOM_UI_DIR` left behind by
# the release that shipped a bundle beside it must stop the server rather than be
# ignored, or an operator upgrades and quietly keeps serving nothing but their
# own stale copy. Started on another port, so a refusal is all this can observe.
# `timeout` bounds a run that wrongly starts, and 124 is read as that failure
# rather than as a refusal — the message check below is what decides.
refusal_status=0
timeout 10 env -u LOOM_UI_PROXY LOOM_BIND="127.0.0.1:$((port + 1))" \
  LOOM_DATA_DIR="$tmp/server-ui-dir-set" LOOM_NODE_ID="release-verification" \
  LOOM_UI_DIR="$tmp/ui" "$server" >"$tmp/loom-ui-dir-set.log" 2>&1 || refusal_status=$?
[[ "$refusal_status" -ne 0 && "$refusal_status" -ne 124 ]] ||
  die "loom-server exited $refusal_status with LOOM_UI_DIR set; the app is compiled into the binary, so a bundle path has to be refused"
grep -q 'LOOM_UI_DIR is no longer read' "$tmp/loom-ui-dir-set.log" ||
  die "a server started with LOOM_UI_DIR set failed without explaining the removal: $(head -c 400 "$tmp/loom-ui-dir-set.log")"
note "LOOM_UI_DIR set -> exit $refusal_status, refused by name"

# A JSON response is captured in a file before it is parsed, and the body is
# printed when the request fails. `curl -f` alone throws the body away, and on
# a contract route the body is the whole diagnosis: a 422 names the field the
# request was missing, which is what a shape regression looks like from here.
api() {
  local method="$1" path="$2" out="$3" data="${4:-}" code
  if [[ -n "$data" ]]; then
    code="$(curl -sS -o "$out" -w '%{http_code}' -X "$method" \
      -H 'content-type: application/json' -d "$data" "$base$path")" ||
      die "$method $path could not be reached"
  else
    code="$(curl -sS -o "$out" -w '%{http_code}' -X "$method" "$base$path")" ||
      die "$method $path could not be reached"
  fi
  if [[ "$code" != 2?? ]]; then
    printf -- '--- %s %s -> %s\n' "$method" "$path" "$code" >&2
    cat "$out" >&2
    printf '\n' >&2
    die "$method $path answered $code"
  fi
}

# The response body as far as it diagnoses anything: a shape mismatch is read
# off what the server actually answered, and an unbounded body is not worth the
# terminal.
body() { head -c 400 "$1" | tr -d '\n'; }

# Daemon self-update (docs/upgrades.md). The two `/install/*` routes are what a
# daemon uses to follow a server whose protocol changed, so a release has to
# prove them on its own artifacts: the binary served, and the digest served with
# it, must both be `loom-daemon` from this build directory. The server was
# started as `$bin_dir/loom-server`, so `$bin_dir` is the artifact directory by
# default — which is the arrangement `deploy/install.sh` produces and the one
# this is here to hold true.
api GET /install/version "$tmp/install-version.json"
served_protocol="$(jq -r '.protocolVersion' "$tmp/install-version.json")"
[[ "$served_protocol" == "$release_protocol" ]] ||
  die "/install/version reports protocol $served_protocol, but --version reports $release_protocol"
note "/install/version ok (protocol $served_protocol)"

served_digest=""
curl -fsS -D "$tmp/artifact.headers" -o "$tmp/loom-daemon.served" \
  "$base/install/loom-daemon?target=$release_target" ||
  die "GET /install/loom-daemon failed"
# Header names are case-insensitive; curl writes them as sent, so the lookup is
# case-folded rather than trusting one spelling.
served_digest="$(tr -d '\r' <"$tmp/artifact.headers" |
  sed -n 's/^[Xx]-[Ll]oom-[Aa]rtifact-[Ss]ha256: *//p' | head -n 1)"
[[ -n "$served_digest" ]] || die "the artifact response has no X-Loom-Artifact-Sha256 header"
[[ "$served_digest" =~ ^[0-9a-f]{64}$ ]] || die "the served digest is not lowercase hex: $served_digest"

# The digest over the bytes the server actually sent, and the digest of the
# `loom-daemon` in this build directory: both must equal the served header. The
# second is the one that matters — it proves the server hosted *this release's*
# daemon and not some other binary that happened to be in the directory.
downloaded_digest="$(sha256sum "$tmp/loom-daemon.served" | cut -d ' ' -f 1)"
[[ "$downloaded_digest" == "$served_digest" ]] ||
  die "the served artifact does not hash to its own header: header $served_digest, body $downloaded_digest"
on_disk_digest="$(sha256sum "$daemon" | cut -d ' ' -f 1)"
[[ "$on_disk_digest" == "$served_digest" ]] ||
  die "the hosted artifact is not this release's loom-daemon: served $served_digest, $daemon is $on_disk_digest"
note "GET /install/loom-daemon -> 200, $served_digest (matches the built loom-daemon)"

# The conditional request: a daemon that already has this digest sends it back
# and must get a 304 with no body, which is what keeps a fleet's reconnects from
# re-downloading megabytes every time.
conditional_code="$(curl -sS -o "$tmp/not-modified" -w '%{http_code}' \
  -H "If-None-Match: \"sha256-$served_digest\"" \
  "$base/install/loom-daemon?target=$release_target")" ||
  die "the conditional artifact request could not be reached"
[[ "$conditional_code" == "304" ]] ||
  die "a conditional artifact request answered $conditional_code, expected 304"
[[ ! -s "$tmp/not-modified" ]] || die "a 304 carried a body"
note "GET /install/loom-daemon (If-None-Match) -> 304"

# The daemon and the server refuse to work together unless their protocol
# versions match, so an enrolled host is that handshake succeeding on real
# sockets. It enrols before the project write because `projects.create` takes
# the contract's body — a name plus the source the project starts with — and the
# only host on this machine is the daemon's.
"$daemon" --server-url "$base" --name "release-verification" \
  --state "$tmp/host-id" >"$tmp/daemon.log" 2>&1 &
daemon_pid=$!
enrolled=""
for _ in $(seq 1 150); do
  curl -fsS "$base/api/v1/hosts" 2>/dev/null |
    jq -e '.[] | select(.status == "connected")' >/dev/null && {
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
# A bare array, like every list route: the shapes here are `projects.list`'s and
# `hosts.list`'s. Each value is read only after the shape it is read from is
# asserted, so a shape change is reported as one instead of as a missing field.
api GET /api/v1/hosts "$tmp/hosts.json"
jq -e 'type == "array"' "$tmp/hosts.json" >/dev/null ||
  die "GET /api/v1/hosts did not answer an array: $(body "$tmp/hosts.json")"
host_id="$(jq -r '[.[] | select(.status == "connected")][0].id // empty' "$tmp/hosts.json")"
[[ -n "$host_id" ]] || die "no connected host in $(body "$tmp/hosts.json")"
note "daemon enrolled as $host_id"

# `projects.create` takes `{ name, source }` and answers 201 with the project
# itself. Both halves of the old assumption are gone: the request carried no
# source, and the response is no longer wrapped in `{ "project": … }`.
api POST /api/v1/projects "$tmp/project.json" \
  "$(jq -cn --arg host "$host_id" --arg path "$tmp/workspace" \
    '{name: "release-verification", source: {type: "local_path", hostId: $host, path: $path}}')"
jq -e 'type == "object" and (.id | type == "string")' "$tmp/project.json" >/dev/null ||
  die "POST /api/v1/projects did not answer a project object: $(body "$tmp/project.json")"
project_id="$(jq -r '.id' "$tmp/project.json")"
# Read it back: the write echoing an id proves the handler ran, the list proves
# the state it published is what a client sees.
api GET /api/v1/projects "$tmp/projects.json"
jq -e 'type == "array"' "$tmp/projects.json" >/dev/null ||
  die "GET /api/v1/projects did not answer an array: $(body "$tmp/projects.json")"
jq -e --arg id "$project_id" '.[] | select(.id == $id)' "$tmp/projects.json" >/dev/null ||
  die "project $project_id was created but is not in the project list"
note "created project $project_id and read it back from the list"

printf 'verified loom %s (%s, protocol %s, commit %s)\n' \
  "$release_version" "$release_target" "$release_protocol" "$release_commit"
