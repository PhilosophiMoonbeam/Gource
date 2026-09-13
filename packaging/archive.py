#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Gource contributors
# SPDX-License-Identifier: GPL-3.0-or-later
"""Create and inspect deterministic native release archives.

The platform wrappers do the build and smoke run.  This helper deliberately
owns only archive mechanics so tar/gzip and zip metadata stay identical when
an archive is rebuilt from the same staged tree.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tarfile
from typing import Iterable, Sequence
import zipfile


ROOT = Path(__file__).resolve().parent.parent
_VERSION_SECTION = re.compile(
    r"(?ms)^\[workspace\.package\]\s*(.*?)(?=^\[|\Z)"
)
_VERSION_VALUE = re.compile(r'(?m)^version\s*=\s*"([^"]+)"\s*$')
_SAFE_VERSION = re.compile(r"^[0-9A-Za-z][0-9A-Za-z._+~-]*$")
_SAFE_TARGET = re.compile(r"^[0-9A-Za-z][0-9A-Za-z._-]*$")

REQUIRED_COMMON = (
    "COPYING",
    "THIRD_PARTY_NOTICES",
    "assets/gource.style",
    "share/man/man1/gource.1",
    "examples/fixtures/single-event.log",
)


class PackagingError(RuntimeError):
    """An actionable packaging or archive-integrity failure."""


def project_version() -> str:
    """Read the workspace package version without network or Cargo metadata."""

    manifest = ROOT / "Cargo.toml"
    try:
        text = manifest.read_text(encoding="utf-8")
    except OSError as error:
        raise PackagingError(f"cannot read {manifest}: {error}") from error
    section = _VERSION_SECTION.search(text)
    if section is None:
        raise PackagingError("Cargo.toml has no [workspace.package] section")
    match = _VERSION_VALUE.search(section.group(1))
    if match is None:
        raise PackagingError("[workspace.package] has no literal version")
    version = match.group(1)
    if _SAFE_VERSION.fullmatch(version) is None:
        raise PackagingError(f"workspace version is not safe for an artifact name: {version!r}")
    return version


def _relative_paths(source: Path) -> list[tuple[Path, str]]:
    """Return all staged directories/files in canonical archive order."""

    if not source.is_dir():
        raise PackagingError(f"staging root is not a directory: {source}")
    entries: list[tuple[Path, str]] = [(source, source.name)]
    for path in source.rglob("*"):
        relative = path.relative_to(source).as_posix()
        archive_name = f"{source.name}/{relative}"
        if path.is_symlink():
            raise PackagingError(f"staging tree contains unsupported symlink: {path}")
        if not path.is_dir() and not path.is_file():
            raise PackagingError(f"staging tree contains unsupported entry: {path}")
        entries.append((path, archive_name))
    entries.sort(key=lambda item: item[1].encode("utf-8"))
    return entries


def _file_mode(path: Path, directory: bool) -> int:
    if directory:
        return 0o755
    try:
        mode = path.stat().st_mode
    except OSError as error:
        raise PackagingError(f"cannot stat staged file {path}: {error}") from error
    return 0o755 if mode & stat.S_IXUSR else 0o644


def _write_tar(source: Path, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    try:
        with output.open("wb") as raw:
            # Supplying every gzip metadata field that can vary keeps the
            # compressed stream reproducible across invocations.
            with gzip.GzipFile(
                fileobj=raw,
                mode="wb",
                filename="",
                mtime=0,
                compresslevel=9,
            ) as compressed:
                with tarfile.open(
                    fileobj=compressed,
                    mode="w",
                    format=tarfile.USTAR_FORMAT,
                ) as archive:
                    for path, archive_name in _relative_paths(source):
                        is_directory = path.is_dir()
                        info = archive.gettarinfo(
                            str(path),
                            arcname=archive_name,
                        )
                        info.mtime = 0
                        info.uid = 0
                        info.gid = 0
                        info.uname = ""
                        info.gname = ""
                        info.mode = _file_mode(path, is_directory)
                        if is_directory:
                            archive.addfile(info)
                        else:
                            with path.open("rb") as input_file:
                                archive.addfile(info, input_file)
    except (OSError, tarfile.TarError) as error:
        raise PackagingError(f"cannot create tar.gz archive {output}: {error}") from error


def _write_zip(source: Path, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    timestamp = (1980, 1, 1, 0, 0, 0)
    try:
        with zipfile.ZipFile(
            output,
            mode="w",
            compression=zipfile.ZIP_DEFLATED,
            compresslevel=9,
            strict_timestamps=True,
        ) as archive:
            for path, archive_name in _relative_paths(source):
                is_directory = path.is_dir()
                name = archive_name + ("/" if is_directory else "")
                info = zipfile.ZipInfo(filename=name, date_time=timestamp)
                info.create_system = 3
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (_file_mode(path, is_directory) & 0xFFFF) << 16
                if is_directory:
                    info.external_attr |= 0x10
                    archive.writestr(info, b"")
                    continue
                with path.open("rb") as input_file, archive.open(info, mode="w") as output_file:
                    while True:
                        chunk = input_file.read(1024 * 1024)
                        if not chunk:
                            break
                        output_file.write(chunk)
    except (OSError, zipfile.BadZipFile, ValueError) as error:
        raise PackagingError(f"cannot create zip archive {output}: {error}") from error


def create_archive(source: Path, output: Path, archive_format: str) -> None:
    """Create a deterministic archive containing ``source`` as its root."""

    if archive_format == "tar.gz":
        _write_tar(source, output)
    elif archive_format == "zip":
        _write_zip(source, output)
    else:
        raise PackagingError(f"unsupported archive format: {archive_format}")


def _archive_names(archive_path: Path, archive_format: str) -> set[str]:
    try:
        if archive_format == "tar.gz":
            with tarfile.open(archive_path, mode="r:gz") as archive:
                names: set[str] = set()
                for member in archive.getmembers():
                    name = member.name.rstrip("/")
                    if name:
                        names.add(name)
                return names
        if archive_format == "zip":
            with zipfile.ZipFile(archive_path, mode="r") as archive:
                return {name.rstrip("/") for name in archive.namelist() if name.rstrip("/")}
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        raise PackagingError(f"cannot inspect archive {archive_path}: {error}") from error
    raise PackagingError(f"unsupported archive format: {archive_format}")


def verify_archive(archive_path: Path, archive_format: str) -> None:
    """Check the archive's self-contained executable and release assets."""

    names = _archive_names(archive_path, archive_format)
    executable = next(
        (name for name in names if name.endswith("/bin/gource-app") or name.endswith("/bin/gource-app.exe")),
        None,
    )
    if executable is None:
        raise PackagingError("archive has no bin/gource-app executable")
    root = executable.rsplit("/bin/", 1)[0]
    required = {f"{root}/{name}" for name in REQUIRED_COMMON}
    missing = sorted(required - names)
    if missing:
        raise PackagingError("archive is missing required entries: " + ", ".join(missing))
    if not archive_path.is_file() or archive_path.stat().st_size == 0:
        raise PackagingError(f"archive is empty: {archive_path}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as input_file:
            while True:
                chunk = input_file.read(1024 * 1024)
                if not chunk:
                    break
                digest.update(chunk)
    except OSError as error:
        raise PackagingError(f"cannot hash {path}: {error}") from error
    return digest.hexdigest()


def verify_diagnose(path: Path) -> None:
    """Validate the stable success fields emitted by the diagnose smoke run."""

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise PackagingError(f"diagnose smoke output is not valid UTF-8 JSON: {error}") from error
    if not isinstance(payload, dict):
        raise PackagingError("diagnose smoke output is not a JSON object")
    if payload.get("snapshots_equal") is not True:
        raise PackagingError("diagnose smoke did not report equal serial/parallel snapshots")
    events = payload.get("events")
    if not isinstance(events, int) or isinstance(events, bool) or events < 1:
        raise PackagingError("diagnose smoke reported no events")
    for key in ("ingest_ms", "serial_ms", "parallel_ms"):
        value = payload.get(key)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or value < 0:
            raise PackagingError(f"diagnose smoke has invalid {key}")


def _archive_format(value: str | None, archive: Path | None) -> str:
    if value:
        return value
    if archive is not None and archive.name.endswith(".tar.gz"):
        return "tar.gz"
    if archive is not None and archive.suffix == ".zip":
        return "zip"
    raise PackagingError("archive format must be supplied for this file name")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--print-version", action="store_true")
    action.add_argument("--archive", action="store_true")
    action.add_argument("--verify-archive", metavar="ARCHIVE", type=Path)
    action.add_argument("--verify-diagnose", metavar="JSON", type=Path)
    action.add_argument("--sha256", metavar="FILE", type=Path)
    parser.add_argument("--format", choices=("tar.gz", "zip"))
    parser.add_argument("--source", type=Path)
    parser.add_argument("--output", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.print_version:
            print(project_version())
            return 0
        if args.archive:
            if args.source is None or args.output is None or args.format is None:
                raise PackagingError("--archive requires --source, --output, and --format")
            create_archive(args.source.resolve(), args.output.resolve(), args.format)
            return 0
        if args.verify_archive is not None:
            archive_format = _archive_format(args.format, args.verify_archive)
            verify_archive(args.verify_archive.resolve(), archive_format)
            return 0
        if args.verify_diagnose is not None:
            verify_diagnose(args.verify_diagnose.resolve())
            return 0
        if args.sha256 is not None:
            print(sha256(args.sha256.resolve()))
            return 0
        raise PackagingError("no packaging operation selected")
    except PackagingError as error:
        print(f"packaging: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
