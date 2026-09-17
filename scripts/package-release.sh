#!/usr/bin/env bash
# Package one target's release binaries for a GitHub Release.
#
# Three things per target: the two bare executables the release page offers as
# `<name>-<target>`, and a tarball an operator extracts and hands to
# `deploy/install.sh`. The tarball's top directory holds the binaries unnamed
# (so `LOOM_BIN_SOURCE=.` points the installer at them), the built product app
# as `ui/` — the bundle `deploy/install.sh` installs and the server serves from
# `LOOM_UI_DIR` — plus `deploy/` and `README.md`.
#
# Every input is a build output or an argument — the binaries from
# `target/<target>/release`, the bundle from `apps/app/dist`, the version from
# `cargo metadata` — so the same command reproduces the same archive on any
# machine that has built both. The binaries are never read: they may be another
# architecture, and the bundle is copied byte for byte for the same reason.
#
# Usage:
#   scripts/package-release.sh <target> [--bin-dir DIR] [--out-dir DIR] [--ui-dir DIR]
#
# --bin-dir defaults to target/<target>/release, --out-dir to dist, --ui-dir to
# apps/app/dist. Requires cargo and jq.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh <target> [--bin-dir DIR] [--out-dir DIR] [--ui-dir DIR]

Options (also arguments):
  <target>          the Rust target triple that was built, e.g.
                    x86_64-unknown-linux-musl
  --bin-dir DIR     build output  default target/<target>/release
  --out-dir DIR     release output  default dist
  --ui-dir DIR      built product app  default apps/app/dist
EOF
}

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

target=""
bin_dir=""
ui_dir=""
out_dir="$repo_root/dist"
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --bin-dir)
      bin_dir="${2:-}"
      shift 2
      ;;
    --out-dir)
      out_dir="${2:-}"
      shift 2
      ;;
    --ui-dir)
      ui_dir="${2:-}"
      shift 2
      ;;
    -*)
      die "unrecognised option: $1"
      ;;
    *)
      [[ -z "$target" ]] || die "unexpected argument: $1"
      target="$1"
      shift
      ;;
  esac
done

[[ -n "$target" ]] || {
  usage >&2
  exit 2
}
[[ -n "$bin_dir" ]] || bin_dir="$repo_root/target/$target/release"
[[ -n "$ui_dir" ]] || ui_dir="$repo_root/apps/app/dist"

for tool in cargo jq tar; do
  command -v "$tool" >/dev/null || die "$tool is required"
done

for binary in loom-server loom-daemon; do
  [[ -x "$bin_dir/$binary" ]] ||
    die "$bin_dir/$binary is missing; build with 'cargo build --release --locked --target $target'"
done

# The app's build output, not the app: a release never compiles the bundle, so a
# checkout that has not run the build fails here rather than shipping a server
# with nothing to serve.
[[ -f "$ui_dir/index.html" ]] ||
  die "no UI bundle at $ui_dir; build one with 'pnpm --filter @bb/app run build', or pass --ui-dir"

# The version comes from the manifest, not from running the binary: an aarch64
# artifact cannot be run on the x86_64 machine that packages it, and the name of
# the archive has to be the same either way.
version="$(
  cargo metadata --manifest-path "$repo_root/Cargo.toml" --no-deps --format-version 1 --locked |
    jq -r '[.packages[] | select(.name == "loom-server")][0].version'
)"
[[ -n "$version" && "$version" != "null" ]] || die "could not read the loom-server version"

name="loom-$version-$target"
staging="$out_dir/$name"

rm -rf "$staging"
install -d -m 0755 "$staging"

# Named for the release page: the asset says which platform it is for.
install -m 0755 "$bin_dir/loom-server" "$out_dir/loom-server-$target"
install -m 0755 "$bin_dir/loom-daemon" "$out_dir/loom-daemon-$target"

# And un-named inside the archive, which is the name `deploy/install.sh`
# installs from its `LOOM_BIN_SOURCE`.
install -m 0755 "$bin_dir/loom-server" "$staging/loom-server"
install -m 0755 "$bin_dir/loom-daemon" "$staging/loom-daemon"
# The bundle keeps the app's own layout: `index.html` at the root of `ui/`, and
# its hashed assets beside it. `deploy/install.sh` installs whatever it finds
# there, so the archive carries the one the server was packaged to serve.
cp -R "$ui_dir" "$staging/ui"
cp -R "$repo_root/deploy" "$staging/deploy"
install -m 0644 "$repo_root/README.md" "$staging/README.md"

# `--owner`/`--group`/`--numeric-owner` keep the build machine's uid out of an
# archive that is extracted by someone else, and `--sort=name` makes the member
# order a property of the tree rather than of the filesystem.
tar -C "$out_dir" --sort=name --owner=0 --group=0 --numeric-owner -czf "$out_dir/$name.tar.gz" "$name"
rm -rf "$staging"

printf 'packaged %s:\n' "$name.tar.gz"
ls -l "$out_dir/loom-server-$target" "$out_dir/loom-daemon-$target" "$out_dir/$name.tar.gz"
