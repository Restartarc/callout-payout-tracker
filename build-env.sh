# Source before building the backfill crate on macOS:  . ./build-env.sh
# - RocksDB's bindgen step loads libclang at runtime; Apple's Command Line Tools ship one.
# - protobuf-src's libtool breaks on spaces in the path, so build outside this folder
#   if its path contains spaces.
export LIBCLANG_PATH=/Library/Developer/CommandLineTools/usr/lib
export DYLD_FALLBACK_LIBRARY_PATH=/Library/Developer/CommandLineTools/usr/lib
case "$PWD" in *" "*) export CARGO_TARGET_DIR="${TMPDIR:-/tmp}/callout-board-target";; esac
