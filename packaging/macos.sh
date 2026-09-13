#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Gource contributors
# SPDX-License-Identifier: GPL-3.0-or-later
set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)

# Separate native tarballs are reproducible on both supported macOS hosts.  An
# explicit --target supplied by the caller is still accepted by the shared
# driver and overrides this host-derived default.
case "$(uname -m)" in
    arm64|aarch64)
        default_target=aarch64-apple-darwin
        ;;
    x86_64|amd64)
        default_target=x86_64-apple-darwin
        ;;
    *)
        printf 'packaging: unsupported macOS architecture: %s\n' "$(uname -m)" >&2
        exit 2
        ;;
esac

exec bash "$script_dir/unix-package.sh" \
    --target "$default_target" \
    --format tar.gz \
    "$@"
