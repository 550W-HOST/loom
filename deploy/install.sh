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
# The binaries must already exist: the script copies them, it never builds.
# Build with `cargo build --release` and pass `--from target/release` (the
# default) or set LOOM_BIN_SOURCE.
#
# Environment overrides: LOOM_INSTALL_PREFIX (/usr/local), LOOM_SYSTEMD_DIR
# (/etc/systemd/system), LOOM_ETC_DIR (/etc/loom), LOOM_STATE_DIR (/var/lib/loom),
# LOOM_SERVICE_USER (loom), LOOM_BIN_SOURCE (target/release).
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_PREFIX="${LOOM_INSTALL_PREFIX:-/usr/local}"
SYSTEMD_DIR="${LOOM_SYSTEMD_DIR:-/etc/systemd/system}"
ETC_DIR="${LOOM_ETC_DIR:-/etc/loom}"
STATE_DIR="${LOOM_STATE_DIR:-/var/lib/loom}"
SERVICE_USER="${LOOM_SERVICE_USER:-loom}"
BIN_SOURCE="${LOOM_BIN_SOURCE:-target/release}"

usage() {
    cat <<'EOF'
Usage: install.sh <command> [arguments]

Commands:
  server
      Install and start the control plane. Requires an already-built
      loom-server and (unless LOOM_BIND is edited) leaves it on loopback.

  daemon <server-key> <server-url> [<host-name>]
      Install and start one execution-daemon instance joined to <server-url>.
      <server-key> names the instance; <host-name> is the display name shown in
      the UI (defaults to <server-key>).

  all <server-key> <server-url> [<host-name>]
      server, then a daemon on the same machine, for a single-box deployment.

  help
      Print this text.

Options (environment variables):
  LOOM_INSTALL_PREFIX  binaries      default /usr/local
  LOOM_BIN_SOURCE      build output  default target/release
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
re-running keeps your edits and only refreshes binaries and units.
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

install_binaries() {
    local source
    source="$(cd "$SCRIPT_DIR/.." && pwd)/$BIN_SOURCE"
    [ -d "$source" ] || die "no build output at $source; run 'cargo build --release' or set LOOM_BIN_SOURCE"
    install -d -m 0755 "$INSTALL_PREFIX/bin"
    local binary
    for binary in loom-server loom-daemon; do
        [ -x "$source/$binary" ] || die "$source/$binary is missing; run 'cargo build --release'"
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
        printf '\nStart the server by hand with:\n  set -a; . %s/loom-server.env; set +a\n  %s/bin/loom-server\n' \
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
