#!/bin/sh
# Creates a test tree with a regular file, a directory, and a FIFO.
set -e
cd "$(dirname "$0")"
rm -rf tree && mkdir -p tree/sub
echo x > tree/a.txt
mkfifo tree/pipe
