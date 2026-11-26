#!/usr/bin/env bash
# Kept for old command lines: forwards to build-wine.sh. Pass --server-only to
# build just the server, as this script used to.
exec "$(dirname "${BASH_SOURCE[0]}")/build-wine.sh" "$@"
