#!/bin/sh
set -eu

fail() {
  printf 'graftr build: %s\n' "$1" >&2
  exit 1
}

source_path=$(realpath -- "$0")
project_path=$(dirname -- "$source_path")
cd "$project_path"

# Select an explicit native target and cache directory; no JSON parser is needed.
version=$("${RUSTC:-rustc}" -vV) || fail 'Could not identify the Rust compiler; previous installation kept'
host=$(printf '%s\n' "$version" | sed -n 's/^host: //p')
case "$host" in
  ''|*[!a-zA-Z0-9_.-]*) fail 'Invalid Rust host target; previous installation kept' ;;
esac
target_dir=${CARGO_TARGET_DIR-"$project_path/target"}
case "$target_dir" in
  '') fail 'CARGO_TARGET_DIR must not be empty; previous installation kept' ;;
  /*) ;;
  *) target_dir="$project_path/$target_dir" ;;
esac
if ! cargo build --manifest-path "$project_path/Cargo.toml" --release --locked \
  --bin graftr --target "$host" --target-dir "$target_dir" >&2; then
  fail 'Native build failed; previous installation kept'
fi

outfile="$project_path/dist/graftr"
mkdir -p -- "$project_path/dist"
staging=$(mktemp -d "$project_path/dist/.graftr-native.XXXXXXXX")
cleanup() { rm -rf -- "$staging"; }
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# A staged, checked copy is renamed on the same filesystem: never truncate a live worker.
cp -- "$target_dir/$host/release/graftr" "$staging/graftr"
chmod 755 "$staging/graftr"
if ! "$staging/graftr" --help >/dev/null; then
  fail 'Native executable smoke check failed; previous installation kept'
fi
mv -fT -- "$staging/graftr" "$outfile"
printf '%s\n' "$outfile"
