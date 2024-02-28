#!/bin/sh

#: Shorthand script to run test with logging setup correctly.

RUST_LOG=${RUST_LOG:-juliet=warn}
export RUST_LOG

# Run one thread at a time to not get interleaved output.
exec cargo -q test --features tracing -- --test-threads=1 --nocapture --format=pretty $@
