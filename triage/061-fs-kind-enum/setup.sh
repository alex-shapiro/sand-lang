#!/bin/sh
set -e
cd "$(dirname "$0")"
rm -rf tree && mkdir -p tree/sub
echo x > tree/a.txt
mkfifo tree/pipe
