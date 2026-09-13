#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Generate a deterministic Gource custom log without committing a large blob."""

from __future__ import annotations

import argparse
import json
import random
import sys
from pathlib import Path
from typing import TextIO


ACTIONS = ("M", "M", "M", "D")
EXTENSIONS = (".rs", ".cpp", ".h", ".md", ".toml", ".txt")


def load_config(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as handle:
        config = json.load(handle)
    if not isinstance(config, dict) or config.get("manifest_version") != 1:
        raise ValueError("config must be a manifest_version 1 object")
    phases = config.get("phases")
    if not isinstance(phases, list) or not phases:
        raise ValueError("config must define at least one phase")
    return config


def build_paths(config: dict) -> list[str]:
    directories = config["top_level_directories"]
    per_directory = int(config["files_per_directory"])
    if per_directory < 1 or not directories:
        raise ValueError("top_level_directories and files_per_directory must be non-empty")
    paths: list[str] = []
    for directory in directories:
        for index in range(per_directory):
            extension = EXTENSIONS[index % len(EXTENSIONS)]
            paths.append(f"{directory}/file-{index:03d}{extension}")
    return paths


def contributors(config: dict) -> list[str]:
    count = int(config["contributor_count"])
    if count < 1:
        raise ValueError("contributor_count must be positive")
    return [f"contributor-{index:02d}" for index in range(count)]


def emit(config: dict, seed: int, output: TextIO) -> int:
    rng = random.Random(seed)
    paths = build_paths(config)
    people = contributors(config)
    probability = float(config.get("same_timestamp_probability", 0.0))
    max_step = int(config.get("timestamp_step_max_seconds", 1))
    idle_every = int(config.get("idle_gap_every_events", 0))
    idle_seconds = int(config.get("idle_gap_seconds", 0))
    if not 0.0 <= probability <= 1.0 or max_step < 1:
        raise ValueError("timestamp settings are out of range")
    if idle_every < 0 or idle_seconds < 0:
        raise ValueError("idle settings cannot be negative")

    active: set[str] = set()
    timestamp = int(config["start_timestamp"])
    source_sequence = 0
    phase_offset = 0

    for phase in config["phases"]:
        name = str(phase["name"])
        events = int(phase["events"])
        visible = int(phase["visible_files"])
        if events < 1 or visible < 1 or visible > len(paths):
            raise ValueError(f"invalid phase {name!r}")
        # Keep each phase's candidate set stable, while allowing a smaller tail
        # to drain files selected by an earlier high-visibility phase.
        candidate_paths = paths[:visible]
        for phase_index in range(events):
            if idle_every and source_sequence and source_sequence % idle_every == 0:
                timestamp += idle_seconds
            elif source_sequence and rng.random() >= probability:
                timestamp += rng.randint(1, max_step)

            outside = sorted(active.difference(candidate_paths))
            if outside:
                path = outside[0]
                action = "D"
                active.remove(path)
            elif len(active) < visible:
                available = [path for path in candidate_paths if path not in active]
                path = available[rng.randrange(len(available))]
                action = "A"
                active.add(path)
            else:
                path = candidate_paths[rng.randrange(len(candidate_paths))]
                action = rng.choice(ACTIONS)
                if action == "D":
                    active.discard(path)
                else:
                    active.add(path)

            person = people[rng.randrange(len(people))]
            output.write(f"{timestamp}|{person}|{action}|{path}\n")
            source_sequence += 1
            phase_offset += 1

    return source_sequence


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config",
        type=Path,
        default=Path(__file__).with_name("large_history.config.json"),
    )
    parser.add_argument("--output", type=Path, help="write the log here; default is stdout")
    parser.add_argument("--seed", type=int, help="override the seed in the config manifest")
    args = parser.parse_args(argv)
    config = load_config(args.config)
    seed = int(config["seed"] if args.seed is None else args.seed)

    if args.output is None:
        emit(config, seed, sys.stdout)
        return 0

    with args.output.open("w", encoding="utf-8", newline="\n") as handle:
        emit(config, seed, handle)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
