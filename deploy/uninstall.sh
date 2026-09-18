#!/usr/bin/env bash
#
# Uninstall loom systemd services. Data is kept unless --purge is given, so the
# default is reversible: reinstall and the daemon resumes its enrolled identity
# and replay cursor.
#
#   sudo deploy/uninstall.sh server [--purge]
#   sudo deploy/uninstall.sh daemon <server-key> [--purge]
#   sudo deploy/uninstall.sh all    <server-key> [--purge]
#   sudo deploy/uninstall.sh binaries
#
# --purge deletes the relay log / the daemon's host id and cursor under
# LOOM_STATE_DIR. It is never inferred: the flag must be passed.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_PREFIX="${LOOM_INSTALL_PREFIX:-/usr/local}"
SYSTEMD_DIR="${LOOM_SYSTEMD_DIR:-/etc/systemd/system}"
ETC_DIR="${LOOM_ETC_DIR:-/etc/loom}"
STATE_DIR="${LOOM_STATE_DIR:-/var/lib/loom}"

usage() {
    cat <<'EOF'
Usage: uninstall.sh <command> [--purge]

Commands:
  server [--purge]            stop and remove the control plane unit
  daemon <server-key> [--purge]  stop and remove one daemon instance
  all    <server-key> [--purge]  server plus one daemon instance
  binaries                    remove /usr/local/bin/loom and its two names
  help                        print this text

--purge also deletes the data directory:
  server          $LOOM_STATE_DIR/server
  daemon <key>    $LOOM_STATE_DIR/machines/<key>

Without --purge the relay log, the enrolled host id and the replay cursor are
left in place, so a reinstall continues where this left off.
EOF
}

log()  { printf '  %s\n' "$*"; }
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

remove_unit() {
    local unit="$1"
    [ -e "$SYSTEMD_DIR/$unit" ] || return 0
    rm -f "$SYSTEMD_DIR/$unit"
    log "removed $SYSTEMD_DIR/$unit"
    systemctl_do daemon-reload
}

uninstall_server() {
    local purge="$1"
    systemctl_do disable --now loom-server.service || true
    remove_unit loom-server.service
    if [ "$purge" = "1" ]; then
        rm -rf "${STATE_DIR:?}/server"
        log "purged $STATE_DIR/server"
    else
        log "kept $STATE_DIR/server"
    fi
    log "kept $ETC_DIR/loom-server.env (delete it by hand if the host is being reused)"
}

uninstall_daemon() {
    local key="$1" purge="$2"
    [ -n "$key" ] || die "daemon needs a <server-key>"
    systemctl_do disable --now "loom-host-daemon@$key.service" || true
    if [ "$purge" = "1" ]; then
        rm -rf "${STATE_DIR:?}/machines/${key:?}"
        rm -f "${ETC_DIR:?}/daemon/${key:?}.env"
        log "purged $STATE_DIR/machines/$key and $ETC_DIR/daemon/$key.env"
    else
        log "kept $STATE_DIR/machines/$key and $ETC_DIR/daemon/$key.env"
    fi
}

uninstall_binaries() {
    # The two names first, then the file they point at, so nothing is ever left
    # dangling over the file being already gone.
    for binary in loom-server loom-daemon loom; do
        rm -f "$INSTALL_PREFIX/bin/$binary"
        log "removed $INSTALL_PREFIX/bin/$binary"
    done
}

command="${1:-help}"
shift || true
purge=0
args=()
for arg in "$@"; do
    case "$arg" in
        --purge) purge=1 ;;
        *) args+=("$arg") ;;
    esac
done

case "$command" in
    server) uninstall_server "$purge" ;;
    daemon) uninstall_daemon "${args[0]:-}" "$purge" ;;
    all)
        uninstall_server "$purge"
        uninstall_daemon "${args[0]:-}" "$purge"
        ;;
    binaries) require_root; uninstall_binaries ;;
    help | --help | -h) usage ;;
    *) usage >&2; die "unknown command: $command" ;;
esac
