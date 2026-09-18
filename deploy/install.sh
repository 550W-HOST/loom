#!/usr/bin/env bash
#
# Install loom as systemd services. Idempotent: re-running upgrades the
# binary and the units and leaves existing environment files and data alone.
#
#   sudo deploy/install.sh server
#   sudo deploy/install.sh worker <server-key> <server-url> [<host-name>]
#   sudo deploy/install.sh all    <server-key> <server-url> [<host-name>]
#
# `<server-key>` is the systemd instance name from deploy/README.md: the name of
# the machine that runs the control plane (a hostname, not a URL). It names the
# worker instance and its data directory.
#
# The script never builds. The binary comes from one of two places:
#
#   --release <version>   download loom-<target> and SHA256SUMS from a GitHub
#                         Release, verify the download, then install it
#   LOOM_BIN_SOURCE       copy it from a local build (default target/release),
#                         i.e. `cargo build --release -p loom` first
#
# So an execution machine with no Rust toolchain is one command away from a
# release artifact, and a machine with a checkout keeps building locally.
#
# An install is that one file, the two names it answers to and the units, and
# nothing else: `loom` is the artifact, and `loom-server` / `loom-worker` are
# relative symlinks to it, so one process can be started as either role and the
# server hosts the sibling `loom-worker` name a self-updating worker fetches. The
# server carries the product app compiled into it (`crates/server/build.rs`), so
# there is no bundle to place beside the binary or to point a variable at.
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
      Install and start the control plane. The binary comes from a local
      build (`cargo build --release -p loom`) or from `--release <version>`,
      and the server is left on loopback unless LOOM_BIND is edited.

  worker <server-key> <server-url> [<host-name>]
      Install and start one execution-worker instance joined to <server-url>.
      <server-key> names the instance; <host-name> is the display name shown in
      the UI (defaults to <server-key>).

  all <server-key> <server-url> [<host-name>]
      server, then a worker on the same machine, for a single-box deployment.

  help
      Print this text.

Options:
  --release <version>
      Take the binary from the GitHub Release for <version>, which is a tag
      (`v0.1.0`, `0.1.0`) or `latest`, instead of copying a local build. The
      release asset for this machine's target, `loom-<target>`, is downloaded
      and its SHA-256 is checked against the release's SHA256SUMS before
      anything is installed.

  --from <dir>
      Copy the binary from <dir> instead of target/release (the same thing as
      setting LOOM_BIN_SOURCE). <dir> may name it either `loom`, as a build
      output does, or `loom-<target>`, as an extracted archive does.

Options (environment variables):
  LOOM_INSTALL_PREFIX  install prefix default /usr/local
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
re-running keeps your edits and only refreshes the binary, its two names and the
units.
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
# A release publishes, per target triple, the one executable plus a
# `sha256sum`-format SHA256SUMS file:
#
#   loom-<target>   SHA256SUMS
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
# the machine with the binary it already had.
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
# itself is not a `download_verified` call: it is the root of trust the asset is
# checked against, and it arrives over the same TLS connection.
download_verified() { # <tag> <asset> <dir>
    download "$1" "$2" "$3"
    verify_checksum "$3" "$2"
}

# Leaves the verified binary in STAGING_DIR under the name a build output uses,
# so the install step is the same as for a local build.
fetch_release() {
    local tag target
    tag="$(release_tag)"
    target="$(target_triple)"
    STAGING_DIR="$(mktemp -d)"
    log "downloading $RELEASE_REPO release $tag for $target"
    download "$tag" "SHA256SUMS" "$STAGING_DIR"
    download_verified "$tag" "loom-$target" "$STAGING_DIR"
    mv "$STAGING_DIR/loom-$target" "$STAGING_DIR/loom"
    chmod 0755 "$STAGING_DIR/loom"
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

# The binary as <dir> names it. A build output holds `loom`; an extracted
# release archive holds `loom-<target>`, the name the release page publishes.
# Both are the same file under a different name, and a local build is tried
# first because that is the common case.
source_binary() { # <dir> -> path
    local dir="$1" asset
    if [ -f "$dir/loom" ]; then
        printf '%s' "$dir/loom"
        return
    fi
    # Asked for only now: a local build names the file `loom` on any platform,
    # while the release name needs the target triple to be detected (and is a
    # Linux-only name, which is why the detection is not on the path above).
    asset="loom-$(target_triple)"
    if [ -f "$dir/$asset" ]; then
        printf '%s' "$dir/$asset"
        return
    fi
    die "$dir holds no loom binary (looked for loom and $asset)"
}

install_binaries() {
    local source binary
    if [ -n "$RELEASE" ]; then
        fetch_release
        source="$STAGING_DIR"
    else
        source="$(bin_source_dir)"
        [ -d "$source" ] ||
            die "no build output at $source; run 'cargo build --release -p loom', set LOOM_BIN_SOURCE, or install from a release with --release <version>"
    fi
    binary="$(source_binary "$source")"
    install -d -m 0755 "$INSTALL_PREFIX/bin"
    install -m 0755 "$binary" "$INSTALL_PREFIX/bin/loom"
    # Relative, not absolute: the names have to survive the prefix being moved,
    # bind-mounted elsewhere or copied into an image. And `-n` on top of `-f` so
    # re-running over the names an older install left as real files replaces
    # them instead of following them.
    ln -sfn loom "$INSTALL_PREFIX/bin/loom-server"
    ln -sfn loom "$INSTALL_PREFIX/bin/loom-worker"
    log "installed $INSTALL_PREFIX/bin/loom (loom-server, loom-worker)"
}

install_unit() {
    local unit="$1"
    install -d -m 0755 "$SYSTEMD_DIR"
    install -m 0644 "$SCRIPT_DIR/systemd/$unit" "$SYSTEMD_DIR/$unit"
    log "installed $SYSTEMD_DIR/$unit"
    systemctl_do daemon-reload
}

ensure_env_file() {
    local source="$1" destination="$2"
    if [ -e "$destination" ]; then
        log "kept existing $destination"
        return
    fi
    install -d -m 0755 "$(dirname -- "$destination")"
    # Read by systemd as root before it drops to $SERVICE_USER; 0640 root:root
    # keeps any future secret (a Redis URL with a password) out of world read.
    install -m 0640 "$source" "$destination"
    log "created $destination — edit it before the first remote deployment"
}

server_state_dir() { printf '%s/server' "$STATE_DIR"; }

install_server() {
    require_root
    install_binaries
    ensure_service_user
    install_unit loom-server.service
    ensure_env_file "$SCRIPT_DIR/env/loom-server.env" "$ETC_DIR/loom-server.env"
    install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$(server_state_dir)"
    log "created $(server_state_dir)"
    systemctl_do enable loom-server.service
    [ "${LOOM_NO_START:-0}" = "1" ] || systemctl_do restart loom-server.service
    if ! service_manager; then
        printf '\nStart the server by hand with:\n  set -a; . %s/loom-server.env; set +a\n  %s/bin/loom server\n' \
            "$ETC_DIR" "$INSTALL_PREFIX"
    fi
}

default_host_name() {
    local key="$1"
    printf '%s' "${key:-$(hostname -s)}"
}

# Escapes a value for the replacement half of a sed s/// expression.
sed_replacement() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/&/\\&/g' -e 's/|/\\|/g'
}

install_worker() {
    local key="${1:-}" server_url="${2:-}" host_name="${3:-}"
    [ -n "$key" ] || die "worker needs a <server-key> instance name"
    [ -n "$server_url" ] || die "worker needs a <server-url>"
    host_name="$(default_host_name "$host_name")"
    require_root
    install_binaries
    ensure_service_user
    install_unit loom-worker@.service
    local env_file="$ETC_DIR/worker/$key.env"
    if [ ! -e "$env_file" ]; then
        install -d -m 0755 "$ETC_DIR/worker"
        sed -e "s|^LOOM_SERVER_URL=.*|LOOM_SERVER_URL=$(sed_replacement "$server_url")|" \
            -e "s|^LOOM_HOST_NAME=.*|LOOM_HOST_NAME=$(sed_replacement "$host_name")|" \
            -e "s|/var/lib/loom/machines/builder-1|$(sed_replacement "$STATE_DIR")/machines/$(sed_replacement "$key")|g" \
            "$SCRIPT_DIR/env/loom-worker.env" > "$env_file"
        chmod 0640 "$env_file"
        log "created $env_file (server $server_url, host name $host_name)"
    else
        log "kept existing $env_file"
    fi
    install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$STATE_DIR/machines/$key"
    log "created $STATE_DIR/machines/$key"
    systemctl_do enable "loom-worker@$key.service"
    [ "${LOOM_NO_START:-0}" = "1" ] || systemctl_do restart "loom-worker@$key.service"
    if ! service_manager; then
        printf '\nStart the worker by hand with:\n  set -a; . %s; set +a\n  %s/bin/loom worker\n' \
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
    worker) install_worker "$@" ;;
    all)
        key="${1:-}"; url="${2:-}"; name="${3:-}"
        install_server
        install_worker "$key" "$url" "$name"
        ;;
    help | --help | -h) usage ;;
    *) usage >&2; die "unknown command: $command" ;;
esac
