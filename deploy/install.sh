#!/usr/bin/env bash
#
# Install loom as systemd services. Idempotent: re-running upgrades the
# binaries and units and leaves existing environment files and data alone.
#
#   sudo deploy/install.sh server
#   sudo deploy/install.sh daemon <server-key> <server-url> [<host-name>]
#   sudo deploy/install.sh all    <server-key> <server-url> [<host-name>]
#
# `<server-key>` is the systemd instance name from deploy/README.md: the name of
# the machine that runs the control plane (a hostname, not a URL). It names the
# daemon instance and its data directory.
#
# The script never builds. The binaries come from one of two places:
#
#   --release <version>   download loom-server-<target>, loom-daemon-<target>
#                         and SHA256SUMS from a GitHub Release, verify them,
#                         then install them
#   LOOM_BIN_SOURCE       copy them from a local build (default
#                         target/release), i.e. `cargo build --release` first
#
# So an execution machine with no Rust toolchain is one command away from a
# release artifact, and a machine with a checkout keeps building locally.
#
# The server serves the product app from disk and has no client of its own, so
# `server` and `all` also install the built bundle — `apps/app/dist` in a
# checkout, the archive's `ui/` for a release, or `--ui-dir <path>` — at
# <prefix>/share/loom/ui, and point LOOM_UI_DIR in the environment file at it. A
# missing bundle, or an environment file that names none, fails the install: a
# server that cannot serve a UI is not a successful install. The script still
# never builds the bundle itself.
#
# Environment overrides: LOOM_INSTALL_PREFIX (/usr/local), LOOM_SYSTEMD_DIR
# (/etc/systemd/system), LOOM_ETC_DIR (/etc/loom), LOOM_STATE_DIR (/var/lib/loom),
# LOOM_SERVICE_USER (loom), LOOM_BIN_SOURCE (target/release), LOOM_RELEASE_REPO
# (550W-HOST/loom), LOOM_RELEASE_BASE_URL, LOOM_RELEASE_API_BASE, LOOM_TARGET,
# GITHUB_TOKEN (for a private repository).
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_PREFIX="${LOOM_INSTALL_PREFIX:-/usr/local}"
SYSTEMD_DIR="${LOOM_SYSTEMD_DIR:-/etc/systemd/system}"
ETC_DIR="${LOOM_ETC_DIR:-/etc/loom}"
STATE_DIR="${LOOM_STATE_DIR:-/var/lib/loom}"
SERVICE_USER="${LOOM_SERVICE_USER:-loom}"
BIN_SOURCE="${LOOM_BIN_SOURCE:-target/release}"
UI_DIR=""                                    # --ui-dir <path>
RELEASE=""                                   # --release <version|latest>
FROM_FLAG=""                                 # --from was given explicitly
RELEASE_REPO="${LOOM_RELEASE_REPO:-550W-HOST/loom}"
RELEASE_BASE="${LOOM_RELEASE_BASE_URL:-https://github.com/$RELEASE_REPO/releases}"
RELEASE_API="${LOOM_RELEASE_API_BASE:-https://api.github.com}"
RELEASE_TOKEN="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
STAGING_DIR=""

API_HEADERS=("Accept: application/vnd.github+json")
if [ -n "$RELEASE_TOKEN" ]; then
    API_HEADERS+=("Authorization: Bearer $RELEASE_TOKEN")
fi

usage() {
    cat <<'EOF'
Usage: install.sh [options] <command> [arguments]

Commands:
  server
      Install and start the control plane. The binaries come from a local
      build (`cargo build --release`) or from `--release <version>`, the UI
      bundle from `apps/app/dist` (or `--ui-dir`), and the server is left on
      loopback unless LOOM_BIND is edited.

  daemon <server-key> <server-url> [<host-name>]
      Install and start one execution-daemon instance joined to <server-url>.
      <server-key> names the instance; <host-name> is the display name shown in
      the UI (defaults to <server-key>).

  all <server-key> <server-url> [<host-name>]
      server, then a daemon on the same machine, for a single-box deployment.

  help
      Print this text.

Options:
  --release <version>
      Take the binaries from the GitHub Release for <version>, which is a tag
      (`v0.1.0`, `0.1.0`) or `latest`, instead of copying a local build. The
      release asset for this machine's target is downloaded and its SHA-256 is
      checked against the release's SHA256SUMS before anything is installed.

  --from <dir>
      Copy the binaries from <dir> instead of target/release (the same thing as
      setting LOOM_BIN_SOURCE).

  --ui-dir <dir>
      Install the UI bundle from <dir> instead of `apps/app/dist` (a checkout)
      or `ui/` (an extracted release archive). <dir> is what
      `pnpm --filter @bb/app run build` writes; it is copied to
      <prefix>/share/loom/ui, which LOOM_UI_DIR in the environment file points
      at.

Options (environment variables):
  LOOM_INSTALL_PREFIX  install root  default /usr/local (bin/, share/loom/ui)
  LOOM_BIN_SOURCE      build output  default target/release
  LOOM_RELEASE_REPO    release repo  default 550W-HOST/loom
  GITHUB_TOKEN         private repo  token used to read a private release
  LOOM_TARGET          triple         overrides the detected release asset
  LOOM_ETC_DIR         environment   default /etc/loom
  LOOM_STATE_DIR       data          default /var/lib/loom
  LOOM_SYSTEMD_DIR     unit files    default /etc/systemd/system
  LOOM_SERVICE_USER    service user  default loom
  LOOM_NO_START=1      enable but do not start
  LOOM_SERVICE_MANAGER=0
                       skip systemctl entirely; print what to run by hand
                       (for containers and for CI; systemd is the supported
                       production supervisor)

The script must run as root. It never overwrites an existing environment file:
re-running keeps your edits and only refreshes binaries and units — but an
existing file that does not set LOOM_UI_DIR fails the install, because a server
with no bundle to serve refuses to start.
EOF
}

log()  { printf '  %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

require_root() {
    [ "$(id -u)" -eq 0 ] || die "must run as root (try: sudo $0 ...)"
}

service_manager() {
    [ "${LOOM_SERVICE_MANAGER:-1}" = "0" ] && return 1
    command -v systemctl >/dev/null 2>&1 || return 1
    [ -d /run/systemd/system ] || return 1
    return 0
}

systemctl_do() {
    if service_manager; then
        systemctl "$@"
    else
        log "systemctl $*   (skipped: no systemd here)"
    fi
}

ensure_service_user() {
    if id -u "$SERVICE_USER" >/dev/null 2>&1; then
        return
    fi
    log "creating system user $SERVICE_USER"
    useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
}

# --- release artifacts -------------------------------------------------------
#
# A release publishes, per target triple, the two executables plus a
# `sha256sum`-format SHA256SUMS file:
#
#   loom-server-<target>   loom-daemon-<target>   SHA256SUMS
#
# so installing from one needs curl (or wget) and sha256sum (or shasum) and
# nothing else. A public repository is read over plain https URLs; a private one
# goes through the API with GITHUB_TOKEN, because github.com's
# /releases/download/... URLs answer 404 for a private repository even when a
# token is sent.

target_triple() {
    if [ -n "${LOOM_TARGET:-}" ]; then
        printf '%s' "$LOOM_TARGET"
        return
    fi
    [ "$(uname -s)" = "Linux" ] ||
        die "release binaries are Linux-only (this is $(uname -s)); on Windows install under WSL2"
    case "$(uname -m)" in
        x86_64 | amd64) printf 'x86_64-unknown-linux-musl' ;;
        aarch64 | arm64) printf 'aarch64-unknown-linux-musl' ;;
        *) die "no release binary for machine type $(uname -m): the release publishes x86_64 and aarch64 Linux only (set LOOM_TARGET to override)" ;;
    esac
}

# Downloads, or fails. There is deliberately no fallback to whatever binary is
# already installed: a failed download must fail the install, not look like a
# successful upgrade.
fetch() { # <url> <destination> [header ...]
    local url="$1" destination="$2" header
    shift 2
    local headers=()
    if command -v curl >/dev/null 2>&1; then
        for header in "$@"; do headers+=(--header "$header"); done
        curl --fail --silent --show-error --location "${headers[@]}" --output "$destination" "$url"
    elif command -v wget >/dev/null 2>&1; then
        for header in "$@"; do headers+=(--header="$header"); done
        wget --quiet "${headers[@]}" --output-document="$destination" "$url"
    else
        die "downloading a release needs curl or wget"
    fi
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        die "verifying a download needs sha256sum or shasum"
    fi
}

# The expected digest for <asset>, out of a `sha256sum`-format SHA256SUMS.
checksum_for() { # <sums file> <asset>
    sed -n "s/^\([0-9a-fA-F]\{64\}\)[[:space:]]\{1,\}\**$2\$/\1/p" "$1" | head -n 1
}

# Runs before anything is installed, so a truncated or tampered download leaves
# the machine with the binaries it already had.
verify_checksum() { # <dir> <asset>
    local dir="$1" asset="$2" expected actual
    [ -f "$dir/SHA256SUMS" ] || die "$dir/SHA256SUMS is missing from the release"
    expected="$(checksum_for "$dir/SHA256SUMS" "$asset")"
    [ -n "$expected" ] || die "SHA256SUMS has no entry for $asset"
    actual="$(sha256_file "$dir/$asset")"
    if [ "$actual" != "$expected" ]; then
        rm -f "$dir/$asset"
        die "SHA-256 mismatch for $asset: SHA256SUMS says $expected, the download is $actual"
    fi
    log "verified $asset $expected"
}

# One JSON field per line, so the extraction below does not depend on whether
# GitHub pretty-prints the response (it does).
json_fields() {
    sed -e 's/[{},]/\n/g'
}

api_get() { # <path below /repos/<repo>/releases> -> stdout
    local body
    body="$(mktemp)"
    if ! fetch "$RELEASE_API/repos/$RELEASE_REPO/releases/$1" "$body" "${API_HEADERS[@]}"; then
        rm -f "$body"
        if [ -n "$RELEASE_TOKEN" ]; then
            die "cannot read $RELEASE_API/repos/$RELEASE_REPO/releases/$1 (does $RELEASE_REPO have that release?)"
        fi
        die "cannot read $RELEASE_API/repos/$RELEASE_REPO/releases/$1 — set GITHUB_TOKEN if $RELEASE_REPO is private"
    fi
    cat "$body"
    rm -f "$body"
}

api_latest_tag() {
    api_get latest | json_fields | sed -n 's/^ *"tag_name": *"\([^"]*\)"$/\1/p' | head -n 1
}

# The API id of <asset>, which is how a private repository's asset is
# downloaded: /repos/.../releases/assets/<id> with an octet-stream Accept.
# A release object lists every asset as {url, id, node_id, name, …}, and the
# scan below takes the id immediately preceding the matching name.
api_asset_id() { # <tag> <asset>
    local id
    id="$(api_get "tags/$1" | json_fields | sed -n \
        -e "/^ *\"name\": *\"$2\"\$/{x;s/^ *\"id\": *\([0-9]\{1,\}\)\$/\1/p;q;}" \
        -e '/^ *"id": *[0-9]\{1,\}$/h')"
    [ -n "$id" ] || die "release $1 has no asset named $2"
    printf '%s' "$id"
}

release_tag() {
    case "$RELEASE" in
        latest)
            local tag
            tag="$(api_latest_tag)"
            [ -n "$tag" ] || die "cannot resolve the latest release of $RELEASE_REPO"
            printf '%s' "$tag"
            ;;
        v*) printf '%s' "$RELEASE" ;;
        *) printf 'v%s' "$RELEASE" ;;
    esac
}

download() { # <tag> <asset> <dir>
    local tag="$1" asset="$2" dir="$3"
    local destination="$dir/$asset"
    if [ -n "$RELEASE_TOKEN" ]; then
        local id
        id="$(api_asset_id "$tag" "$asset")"
        fetch "$RELEASE_API/repos/$RELEASE_REPO/releases/assets/$id" "$destination" \
            "Authorization: Bearer $RELEASE_TOKEN" "Accept: application/octet-stream" ||
            die "cannot download $asset of release $tag (asset $id)"
    else
        fetch "$RELEASE_BASE/download/$tag/$asset" "$destination" ||
            die "cannot download $RELEASE_BASE/download/$tag/$asset — set GITHUB_TOKEN if $RELEASE_REPO is private"
    fi
}

# Downloads <asset> and proves it is the file SHA256SUMS describes. SHA256SUMS
# itself is not a `download_verified` call: it is the root of trust the others
# are checked against, and it arrives over the same TLS connection.
download_verified() { # <tag> <asset> <dir>
    download "$1" "$2" "$3"
    verify_checksum "$3" "$2"
}

# Leaves both verified binaries in STAGING_DIR, shaped like a build output
# directory, so the install step is the same as for a local build.
fetch_release() {
    local tag target binary
    tag="$(release_tag)"
    target="$(target_triple)"
    STAGING_DIR="$(mktemp -d)"
    log "downloading $RELEASE_REPO release $tag for $target"
    download "$tag" "SHA256SUMS" "$STAGING_DIR"
    for binary in loom-server loom-daemon; do
        download_verified "$tag" "$binary-$target" "$STAGING_DIR"
        mv "$STAGING_DIR/$binary-$target" "$STAGING_DIR/$binary"
        chmod 0755 "$STAGING_DIR/$binary"
    done
}

cleanup_staging() {
    if [ -n "$STAGING_DIR" ] && [ -d "$STAGING_DIR" ]; then
        rm -rf "$STAGING_DIR"
        STAGING_DIR=""
    fi
}

bin_source_dir() {
    case "$BIN_SOURCE" in
        /*) printf '%s' "$BIN_SOURCE" ;;
        *) printf '%s/%s' "$(cd -- "$SCRIPT_DIR/.." && pwd)" "$BIN_SOURCE" ;;
    esac
}

install_binaries() {
    local source binary
    if [ -n "$RELEASE" ]; then
        fetch_release
        source="$STAGING_DIR"
    else
        source="$(bin_source_dir)"
        [ -d "$source" ] ||
            die "no build output at $source; run 'cargo build --release', set LOOM_BIN_SOURCE, or install from a release with --release <version>"
    fi
    install -d -m 0755 "$INSTALL_PREFIX/bin"
    for binary in loom-server loom-daemon; do
        [ -f "$source/$binary" ] || die "$source/$binary is missing"
        install -m 0755 "$source/$binary" "$INSTALL_PREFIX/bin/$binary"
    done
    log "installed binaries to $INSTALL_PREFIX/bin"
}

install_unit() {
    local unit="$1"
    install -d -m 0755 "$SYSTEMD_DIR"
    install -m 0644 "$SCRIPT_DIR/systemd/$unit" "$SYSTEMD_DIR/$unit"
    log "installed $SYSTEMD_DIR/$unit"
    systemctl_do daemon-reload
}

# --- UI bundle --------------------------------------------------------------
#
# The server serves the product app from disk and has no client compiled into
# it, so the bundle is as much a prerequisite as the binary is. This script
# still builds nothing: a bundle that has not been built fails the install with
# the command that builds it.

ui_install_dir() { printf '%s/share/loom/ui' "$INSTALL_PREFIX"; }

# Where the bundle to install comes from: --ui-dir, else the checkout's build
# output, else the `ui/` a release archive carries beside `deploy/`. The archive
# is the fallback rather than a peer, and only when there is no checkout at all,
# so a `ui/` left lying in a working tree can never be mistaken for the app.
ui_source_dir() {
    local root
    if [ -n "$UI_DIR" ]; then
        printf '%s' "$UI_DIR"
        return
    fi
    root="$(cd -- "$SCRIPT_DIR/.." && pwd)"
    if [ -d "$root/apps/app" ]; then
        printf '%s/apps/app/dist' "$root"
    else
        printf '%s/ui' "$root"
    fi
}

install_ui_bundle() { # <bundle directory>
    local source="$1" destination
    destination="$(ui_install_dir)"
    [ -f "$source/index.html" ] ||
        die "no UI bundle at $source; build one with 'pnpm --filter @bb/app run build', or point --ui-dir at a built bundle"
    case "$destination" in
        /*/share/loom/ui) ;;
        *) die "refusing to install a UI bundle to '$destination'" ;;
    esac
    install -d -m 0755 "$(dirname -- "$destination")"
    # Replaced rather than merged: the bundle is a set of content-hashed files,
    # and keeping the previous release's set would grow the directory on every
    # upgrade for nothing.
    rm -rf "$destination"
    cp -R "$source" "$destination"
    # Readable whatever umask built it: the service user reads this tree, and
    # every file in it is served to clients anyway.
    chmod -R a+rX "$destination"
    log "installed UI bundle to $destination"
}

# Escapes a value for the replacement half of a sed s/// expression.
sed_replacement() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/&/\\&/g' -e 's/|/\\|/g'
}

# Whether <file> assigns <name>. systemd reads the file literally, so a
# commented-out line is not an assignment; one written with leading whitespace
# still is.
env_file_sets() { # <file> <name>
    grep -q "^[[:space:]]*$2=" "$1"
}

# An environment file an operator has already edited is never rewritten, so the
# bundle it names is checked instead: an install that quietly left LOOM_UI_DIR
# out would look successful and then fail on the next restart.
require_env_ui_dir() { # <env file> <bundle directory>
    local file="$1" bundle="$2"
    [ -e "$file" ] || return 0
    env_file_sets "$file" LOOM_UI_DIR && return 0
    die "$file does not set LOOM_UI_DIR; add 'LOOM_UI_DIR=$bundle' to it, or delete the file and re-run to have this script write one — the server refuses to start without a UI source"
}

ensure_env_file() { # <template> <destination> <LOOM_UI_DIR value>
    local source="$1" destination="$2" ui_dir="$3"
    if [ -e "$destination" ]; then
        log "kept existing $destination"
        return
    fi
    install -d -m 0755 "$(dirname -- "$destination")"
    # Read by systemd as root before it drops to $SERVICE_USER; 0640 root:root
    # keeps any future secret (a Redis URL with a password) out of world read.
    # The bundle path is the one value an install cannot leave to the template's
    # own default, because the prefix it installs under is a variable.
    sed -e "s|^LOOM_UI_DIR=.*|LOOM_UI_DIR=$(sed_replacement "$ui_dir")|" \
        "$source" > "$destination"
    chmod 0640 "$destination"
    env_file_sets "$destination" LOOM_UI_DIR ||
        die "$SCRIPT_DIR/env/loom-server.env has no LOOM_UI_DIR line to fill in"
    log "created $destination — edit it before the first remote deployment"
}

server_state_dir() { printf '%s/server' "$STATE_DIR"; }

install_server() {
    require_root
    local bundle_dir
    bundle_dir="$(ui_install_dir)"
    # Both UI prerequisites are checked before anything is installed: neither can
    # be repaired by the rest of the install, and a refusal has to leave the
    # machine exactly as it was.
    require_env_ui_dir "$ETC_DIR/loom-server.env" "$bundle_dir"
    install_ui_bundle "$(ui_source_dir)"
    install_binaries
    ensure_service_user
    install_unit loom-server.service
    ensure_env_file "$SCRIPT_DIR/env/loom-server.env" "$ETC_DIR/loom-server.env" "$bundle_dir"
    install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$(server_state_dir)"
    log "created $(server_state_dir)"
    systemctl_do enable loom-server.service
    [ "${LOOM_NO_START:-0}" = "1" ] || systemctl_do restart loom-server.service
    if ! service_manager; then
        printf '\nStart the server by hand with:\n  set -a; . %s/loom-server.env; set +a\n  %s/bin/loom-server\n' \
            "$ETC_DIR" "$INSTALL_PREFIX"
    fi
}

default_host_name() {
    local key="$1"
    printf '%s' "${key:-$(hostname -s)}"
}

install_daemon() {
    local key="${1:-}" server_url="${2:-}" host_name="${3:-}"
    [ -n "$key" ] || die "daemon needs a <server-key> instance name"
    [ -n "$server_url" ] || die "daemon needs a <server-url>"
    host_name="$(default_host_name "$host_name")"
    require_root
    install_binaries
    ensure_service_user
    install_unit loom-host-daemon@.service
    local env_file="$ETC_DIR/daemon/$key.env"
    if [ ! -e "$env_file" ]; then
        install -d -m 0755 "$ETC_DIR/daemon"
        sed -e "s|^LOOM_SERVER_URL=.*|LOOM_SERVER_URL=$(sed_replacement "$server_url")|" \
            -e "s|^LOOM_HOST_NAME=.*|LOOM_HOST_NAME=$(sed_replacement "$host_name")|" \
            -e "s|/var/lib/loom/machines/builder-1|$(sed_replacement "$STATE_DIR")/machines/$(sed_replacement "$key")|g" \
            "$SCRIPT_DIR/env/loom-host-daemon.env" > "$env_file"
        chmod 0640 "$env_file"
        log "created $env_file (server $server_url, host name $host_name)"
    else
        log "kept existing $env_file"
    fi
    install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$STATE_DIR/machines/$key"
    log "created $STATE_DIR/machines/$key"
    systemctl_do enable "loom-host-daemon@$key.service"
    [ "${LOOM_NO_START:-0}" = "1" ] || systemctl_do restart "loom-host-daemon@$key.service"
    if ! service_manager; then
        printf '\nStart the daemon by hand with:\n  set -a; . %s; set +a\n  %s/bin/loom-daemon\n' \
            "$env_file" "$INSTALL_PREFIX"
    fi
}

trap cleanup_staging EXIT

args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --release)
            [ $# -ge 2 ] || die "--release needs a version (for example --release v0.1.0, or --release latest)"
            RELEASE="$2"
            shift 2
            ;;
        --release=*)
            RELEASE="${1#--release=}"
            shift
            ;;
        --from)
            [ $# -ge 2 ] || die "--from needs a directory"
            BIN_SOURCE="$2"
            FROM_FLAG=1
            shift 2
            ;;
        --from=*)
            BIN_SOURCE="${1#--from=}"
            FROM_FLAG=1
            shift
            ;;
        --ui-dir)
            [ $# -ge 2 ] || die "--ui-dir needs a directory"
            UI_DIR="$2"
            shift 2
            ;;
        --ui-dir=*)
            UI_DIR="${1#--ui-dir=}"
            shift
            ;;
        --)
            shift
            while [ $# -gt 0 ]; do
                args+=("$1")
                shift
            done
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            args+=("$1")
            shift
            ;;
    esac
done
[ -z "$RELEASE" ] || [ -z "$FROM_FLAG" ] || die "--release and --from are mutually exclusive"
set -- ${args[@]+"${args[@]}"}

command="${1:-help}"
shift || true
case "$command" in
    server) install_server ;;
    daemon) install_daemon "$@" ;;
    all)
        key="${1:-}"; url="${2:-}"; name="${3:-}"
        install_server
        install_daemon "$key" "$url" "$name"
        ;;
    help | --help | -h) usage ;;
    *) usage >&2; die "unknown command: $command" ;;
esac
