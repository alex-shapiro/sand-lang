#!/usr/bin/env bash
# E15: the borrow checker rejects a raw handle held across a nested call.
cd "$(dirname "$0")"
echo "== rustc $(rustc --version | cut -d' ' -f2)"
echo "-- bad.rs (raw Val across the call):"
rustc --edition 2024 -o /tmp/e15bad bad.rs 2>&1 | grep -E "^error|borrow occurs|later used" | head -6
echo "-- good.rs (rooted Handle across the call):"
rustc --edition 2024 -o /tmp/e15good good.rs 2>&1 | head -3 && /tmp/e15good
