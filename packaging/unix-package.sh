#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Gource contributors
# SPDX-License-Identifier: GPL-3.0-or-later

# Build and package one Unix target.  Linux and macOS entry points pass their
# target and archive format here, while all commands remain explicit argv.
set -Eeuo pipefail

usage() {
    printf 'usage: %s --target TARGET --format tar.gz [--output-dir DIR]\n' "$0" >&2
}

target=''
archive_format=''
output_dir_input='dist'

while (($# > 0)); do
    case "$1" in
        --target)
            if (($# < 2)); then
                usage
                exit 2
            fi
            target=$2
            shift 2
            ;;
        --format)
            if (($# < 2)); then
                usage
                exit 2
            fi
            archive_format=$2
            shift 2
            ;;
        --output-dir)
            if (($# < 2)); then
                usage
                exit 2
            fi
            output_dir_input=$2
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            printf 'packaging: unknown argument: %s\n' "$1" >&2
            usage
            exit 2
            ;;
    esac
done

if [[ -z "$target" || -z "$archive_format" ]]; then
    usage
    exit 2
fi
if [[ ! "$target" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
    printf 'packaging: unsafe target name: %s\n' "$target" >&2
    exit 2
fi
if [[ "$archive_format" != 'tar.gz' ]]; then
    printf 'packaging: Unix packaging requires tar.gz, got: %s\n' "$archive_format" >&2
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
project_root=$(cd "$script_dir/.." && pwd -P)
helper="$project_root/packaging/archive.py"

command -v cargo >/dev/null 2>&1 || {
    printf 'packaging: cargo is required\n' >&2
    exit 1
}
command -v python3 >/dev/null 2>&1 || {
    printf 'packaging: python3 is required\n' >&2
    exit 1
}
command -v rustup >/dev/null 2>&1 || {
    printf 'packaging: rustup is required to install target %s\n' "$target" >&2
    exit 1
}

cd "$project_root"
rustup target add "$target"
cargo build --locked --release --package gource-app --target "$target"

version=$(python3 "$helper" --print-version)
case "$output_dir_input" in
    /*) output_dir=$output_dir_input ;;
    *) output_dir="$project_root/$output_dir_input" ;;
esac
mkdir -p "$output_dir"

# Keep staging private until the archive has passed all checks.  The final
# archive and checksum are ordinary release outputs in the requested directory.
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/gource-package.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT
package_root="$tmp_dir/gource-$version-$target"
mkdir -p \
    "$package_root/bin" \
    "$package_root/assets/fonts" \
    "$package_root/share/man/man1" \
    "$package_root/examples/fixtures"

binary="$project_root/target/$target/release/gource-app"
fixture="$project_root/tests/fixtures/single-event.log"
for required in \
    "$binary" \
    "$project_root/COPYING" \
    "$project_root/THIRD_PARTY_NOTICES" \
    "$project_root/README.md" \
    "$project_root/data/gource.style" \
    "$project_root/data/fonts/README" \
    "$project_root/data/gource.1" \
    "$fixture"; do
    if [[ ! -f "$required" ]]; then
        printf 'packaging: required input is missing: %s\n' "$required" >&2
        exit 1
    fi
done

install -m 0755 "$binary" "$package_root/bin/gource-app"
install -m 0644 "$project_root/COPYING" "$package_root/COPYING"
install -m 0644 "$project_root/THIRD_PARTY_NOTICES" "$package_root/THIRD_PARTY_NOTICES"
install -m 0644 "$project_root/README.md" "$package_root/README.md"
install -m 0644 "$project_root/data/gource.style" "$package_root/assets/gource.style"
install -m 0644 "$project_root/data/fonts/README" "$package_root/assets/fonts/README"
install -m 0644 "$project_root/data/gource.1" "$package_root/share/man/man1/gource.1"
install -m 0644 "$fixture" "$package_root/examples/fixtures/single-event.log"

help_output="$tmp_dir/help.txt"
if ! "$package_root/bin/gource-app" --help >"$help_output" 2>&1; then
    printf 'packaging: --help smoke failed\n' >&2
    cat "$help_output" >&2
    exit 1
fi
if [[ ! -s "$help_output" ]]; then
    printf 'packaging: --help smoke produced no output\n' >&2
    exit 1
fi

diagnose_output="$tmp_dir/diagnose.json"
diagnose_error="$tmp_dir/diagnose.stderr"
if ! "$package_root/bin/gource-app" diagnose \
    --input "$package_root/examples/fixtures/single-event.log" \
    --threads 1 >"$diagnose_output" 2>"$diagnose_error"; then
    printf 'packaging: diagnose smoke failed\n' >&2
    cat "$diagnose_error" >&2
    exit 1
fi
python3 "$helper" --verify-diagnose "$diagnose_output"

archive_name="gource-$version-$target.tar.gz"
archive_path="$output_dir/$archive_name"
python3 "$helper" \
    --archive \
    --format "$archive_format" \
    --source "$package_root" \
    --output "$archive_path"
python3 "$helper" \
    --verify-archive "$archive_path" \
    --format "$archive_format"
checksum=$(python3 "$helper" --sha256 "$archive_path")
printf '%s  %s\n' "$checksum" "$archive_name" >"$output_dir/$archive_name.sha256"
printf 'packaging: wrote %s and %s.sha256\n' "$archive_path" "$archive_path"
