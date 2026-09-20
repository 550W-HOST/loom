#!/usr/bin/env bash
# Package one target's release binary for a GitHub Release.
#
# Two things per target: the bare executable the release page offers as
# `loom-<target>`, and a tarball. The tarball's top directory holds that same
# `loom-<target>` name — one name for the asset and for the file inside it —
# plus `SHA256SUMS` over that file and `README.md`. There is no separate
# deployment directory any more: deployment is the one binary and its
# subcommands. No client ships beside it either: the product app is compiled
# into the server, so the binary that serves it is the binary in the archive,
# and the two roles (`loom server`, `loom worker`) are that one file.
#
# Every input is a build output or an argument — the binary from
# `target/<target>/release`, the version from `cargo metadata` — so the same
# command reproduces the same archive on any machine that has built it. The
# binary is never read: it may be another architecture.
#
# Usage:
#   scripts/package-release.sh <target> [--bin-dir DIR] [--out-dir DIR]
#
# --bin-dir defaults to target/<target>/release, --out-dir to dist. Requires
# cargo, jq and sha256sum.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh <target> [--bin-dir DIR] [--out-dir DIR]

Options (also arguments):
  <target>          the Rust target triple that was built, e.g.
                    x86_64-unknown-linux-musl
  --bin-dir DIR     build output  default target/<target>/release
  --out-dir DIR     release output  default dist
EOF
}

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

target=""
bin_dir=""
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

for tool in cargo jq sha256sum tar; do
  command -v "$tool" >/dev/null || die "$tool is required"
done

[[ -x "$bin_dir/loom" ]] ||
  die "$bin_dir/loom is missing; build with 'cargo build --release --locked -p loom --target $target'"

# The version comes from the manifest, not from running the binary: an aarch64
# artifact cannot be run on the x86_64 machine that packages it, and the name of
# the archive has to be the same either way.
version="$(
  cargo metadata --manifest-path "$repo_root/Cargo.toml" --no-deps --format-version 1 --locked |
    jq -r '[.packages[] | select(.name == "loom")][0].version'
)"
[[ -n "$version" && "$version" != "null" ]] || die "could not read the loom version"

name="loom-$version-$target"
staging="$out_dir/$name"

rm -rf "$staging"
install -d -m 0755 "$staging"

# Named for the release page: the asset says which platform it is for.
install -m 0755 "$bin_dir/loom" "$out_dir/loom-$target"

# And inside the archive under the same name, so an extracted directory holds
# the executable an operator runs as `./loom-<target> server` or `... worker`:
# one name for the asset and for the file inside it.
install -m 0755 "$bin_dir/loom" "$staging/loom-$target"
install -m 0644 "$repo_root/README.md" "$staging/README.md"

# A checksum of the one file in here, written inside the staging directory so it
# names `loom-<target>` relative to the archive root and `sha256sum -c
# SHA256SUMS` works after extraction. (The release page's own SHA256SUMS is
# assembled later over every target's assets; this one is per archive.)
(
  cd "$staging"
  sha256sum -- "loom-$target" >SHA256SUMS
)

# `--owner`/`--group`/`--numeric-owner` keep the build machine's uid out of an
# archive that is extracted by someone else, and `--sort=name` makes the member
# order a property of the tree rather than of the filesystem.
tar -C "$out_dir" --sort=name --owner=0 --group=0 --numeric-owner -czf "$out_dir/$name.tar.gz" "$name"
rm -rf "$staging"

printf 'packaged %s:\n' "$name.tar.gz"
ls -l "$out_dir/loom-$target" "$out_dir/$name.tar.gz"
