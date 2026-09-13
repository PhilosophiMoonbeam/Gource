#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Gource contributors
# SPDX-License-Identifier: GPL-3.0-or-later
set -Eeuo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec bash "$script_dir/unix-package.sh" \
    --target x86_64-unknown-linux-gnu \
    --format tar.gz \
    "$@"
