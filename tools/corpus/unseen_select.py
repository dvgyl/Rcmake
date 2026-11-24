#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Sample a frozen repository pool and write a metadata-only TOML manifest."""

import argparse
from hashlib import sha256
import json
from pathlib import Path

from unseen_pool import COMMIT, CORPUS, REPO_NAME, exclusions


def validate_pin(name, url, commit):
    if (not REPO_NAME.fullmatch(name) or name.split("/")[1] in {".", ".."}
            or url != f"https://github.com/{name}" or not COMMIT.fullmatch(commit)):
        raise ValueError("Invalid GitHub repository name, URL, or commit.")


def rank(seed, name):
    # Hash the seed and repository name to make the order independent of Python's PRNG.
    return sha256(f"{seed}:{name}".encode()).digest(), name


def manifest(pool_bytes, seed, count):
    pool = {}
    excluded_names, excluded_repos = exclusions()
    entries = json.loads(pool_bytes)
    if not isinstance(entries, list):
        raise ValueError("The pool must contain a JSON array.")
    for entry in entries:
        name, url, commit = (entry[key] for key in ("name", "url", "head_sha"))
        validate_pin(name, url, commit)
        if not isinstance(entry["topic_group"], str) or not entry["topic_group"]:
            raise ValueError(f"Repository {name} has no category.")
        key = name.lower()
        if key in pool:
            raise ValueError(f"Duplicate repository {name}.")
        if (key in excluded_repos or key in excluded_names
                or key.split("/")[-1] in excluded_names):
            raise ValueError(f"Repository {name} is already in the corpus.")
        pool[key] = entry
    if count > len(pool):
        raise ValueError(f"Pool has {len(pool)} candidates. Set --n to {len(pool)} or less.")

    def quoted(value):
        return json.dumps(value, ensure_ascii=False)

    lines = [
        "# SPDX-License-Identifier: MIT OR Apache-2.0",
        "# Frozen sample from the GitHub pool. See tools/corpus/unseen_select.py.",
        "# The first fifth of the seeded order is the development split.",
        "schema_version = 1",
        f"seed = {seed}",
        f"pool_size = {len(pool)}",
        f'pool_sha256 = "{sha256(pool_bytes).hexdigest()}"',
    ]
    for index, key in enumerate(sorted(pool, key=lambda name: rank(seed, name))[:count]):
        entry = pool[key]
        split = "dev" if index < count // 5 else "holdout"
        lines.extend(("", "[[project]]", f'name = {quoted(entry["name"])}',
                      f'url = {quoted(entry["url"])}', f'commit = {quoted(entry["head_sha"])}',
                      f'split = "{split}"', f'category = {quoted(entry["topic_group"])}'))
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pool", type=Path, help="Read the frozen JSON pool from this path.")
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--n", type=int, default=300)
    parser.add_argument("--out", type=Path, default=CORPUS / "unseen.toml")
    args = parser.parse_args()
    if args.n <= 0 or not -(2**63) <= args.seed < 2**63:
        parser.error("Use a positive --n and a signed 64-bit --seed.")
    try:
        args.out.write_text(manifest(args.pool.read_bytes(), args.seed, args.n), encoding="utf-8")
    except (OSError, KeyError, TypeError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
