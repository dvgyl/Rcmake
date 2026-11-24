#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Save a GitHub repository pool with verified default-branch commits."""

import argparse
from datetime import date, datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import time
import tomllib
from urllib.parse import urlsplit

CORPUS = Path(__file__).resolve().parents[2] / "conformance/corpus"
REPO_NAME = re.compile(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+")
COMMIT = re.compile(r"[0-9a-f]{40}")
QUERY = """
query($search: String!, $after: String) {
  search(type: REPOSITORY, query: $search, first: 25, after: $after) {
    nodes {
      ... on Repository {
        nameWithOwner url isFork isArchived isPrivate stargazerCount pushedAt
        primaryLanguage { name }
        repositoryTopics(first: 10) { nodes { topic { name } } }
        defaultBranchRef {
          name
          target { ... on Commit { oid file(path: "CMakeLists.txt") { type mode } } }
        }
      }
    }
    pageInfo { hasNextPage endCursor }
  }
}
"""
TOPICS = {
    "graphics": {"graphics", "rendering", "opengl", "vulkan", "image-processing"},
    "games": {"game", "game-engine", "gamedev", "emulator"},
    "data": {"database", "compression", "serialization", "parser", "json"},
    "network": {"network", "networking", "http", "https", "mqtt", "websocket"},
    "science": {"robotics", "machine-learning", "computer-vision", "math", "simulation"},
    "systems": {"embedded", "firmware", "operating-system", "compiler", "filesystem"},
}


def repository_url(url):
    parsed = urlsplit(url)
    if parsed.hostname in {"github.com", "codeload.github.com"}:
        parts = parsed.path.strip("/").split("/")
        if len(parts) >= 2:
            return f"{parts[0]}/{parts[1].removesuffix('.git')}".lower()
    return None


def tables(data):
    """Yield nested tables, including test dependencies and prerequisites."""
    if isinstance(data, dict):
        yield data
        for value in data.values():
            yield from tables(value)
    elif isinstance(data, list):
        for item in data:
            yield from tables(item)


def exclusions():
    names, repos = set(), set()
    for filename in ("manifest.toml", "boss_fight.toml"):
        with (CORPUS / filename).open("rb") as file:
            data = tomllib.load(file)
        names.update(project["name"].lower() for project in data["project"])
        for table in tables(data):
            if repo := table.get("repository"):
                repos.add(repo.lower())
            if url := table.get("url"):
                if repo := repository_url(url):
                    repos.add(repo)
    return names, repos


def missing_file(error):
    path = error.get("path", [])
    return (error.get("type") == "NOT_FOUND"
            and path[:2] == ["search", "nodes"]
            and path[3:] == ["defaultBranchRef", "target", "file"])


def search_page(query, cursor):
    command = ["gh", "api", "graphql", "-f", f"query={QUERY}", "-f", f"search={query}"]
    if cursor:
        command.extend(("-f", f"after={cursor}"))
    for attempt in range(4):
        try:
            result = subprocess.run(command, capture_output=True, text=True, timeout=60)
        except subprocess.TimeoutExpired:
            if attempt == 3:
                raise ValueError("GitHub API request timed out.") from None
        else:
            try:
                response = json.loads(result.stdout)
            except json.JSONDecodeError:
                response = {}
            errors = response.get("errors", [])
            if (response.get("data") or {}).get("search") and (
                result.returncode == 0 or (errors and all(map(missing_file, errors)))
            ):
                return response["data"]["search"]
            detail = result.stderr.strip() or str(errors)
            retryable = re.search(
                r"rate.?limit|HTTP (429|50[234])|timed? ?out", detail, re.IGNORECASE
            )
            if not retryable or attempt == 3:
                raise ValueError(f"GitHub API search failed. {detail}")
        # Bound the wait so a full rate-limit window can be retried later.
        time.sleep(5 * 2**attempt)


def candidates(language, cutoff, pages):
    search = (f"is:public fork:false archived:false language:{language} "
              f"stars:>=50 pushed:>={cutoff} sort:stars-desc")
    cursor = None
    for _ in range(pages):
        try:
            result = search_page(search, cursor)
        except ValueError as error:
            raise ValueError(f"{language} page {_ + 1}. {error}") from error
        yield from result["nodes"]
        if not result["pageInfo"]["hasNextPage"]:
            break
        cursor = result["pageInfo"]["endCursor"]


def pool_entry(node, language, cutoff, excluded_names, excluded_repos):
    if not node or not node.get("defaultBranchRef") or not node.get("primaryLanguage"):
        return None
    name = node["nameWithOwner"]
    owner_repo = name.lower()
    branch = node["defaultBranchRef"]
    target = branch["target"]
    file = target.get("file") or {}
    if (owner_repo in excluded_repos or owner_repo in excluded_names
            or owner_repo.split("/")[-1] in excluded_names
            or node["isPrivate"] or node["isFork"] or node["isArchived"]
            or node["primaryLanguage"]["name"] != language
            or node["stargazerCount"] < 50 or node["pushedAt"][:10] < cutoff
            or file.get("type") != "blob" or file.get("mode") not in {0o100644, 0o100755}
            or not COMMIT.fullmatch(target.get("oid", ""))):
        return None

    topics = {entry["topic"]["name"] for entry in node["repositoryTopics"]["nodes"]}
    group = next((group for group, tags in TOPICS.items() if topics & tags), "other")
    return {
        "name": name, "url": node["url"], "default_branch": branch["name"],
        "head_sha": target["oid"], "stars": node["stargazerCount"],
        "language": language, "topic_group": group,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True, help="Write the JSON pool to this path.")
    parser.add_argument("--since", type=date.fromisoformat, help="Use this UTC push date cutoff.")
    parser.add_argument("--pages", type=int, default=28,
                        help="Search this many 25-result pages per language in descending star order.")
    parser.add_argument("--exclude", default=[], action="append", metavar="OWNER/REPO",
                        help="Exclude a repository already used in a compatibility run.")
    args = parser.parse_args()
    if not 1 <= args.pages <= 40:
        parser.error("--pages must be between 1 and 40.")
    if any(not REPO_NAME.fullmatch(name) for name in args.exclude):
        parser.error("--exclude must use OWNER/REPO.")
    today = datetime.now(timezone.utc).date()
    day = 28 if (today.month, today.day) == (2, 29) else today.day
    cutoff = args.since or today.replace(year=today.year - 2, day=day)
    try:
        excluded_names, excluded_repos = exclusions()
        excluded_repos.update(map(str.lower, args.exclude))
        pool = {}
        for language in ("C", "C++"):
            for node in candidates(language, cutoff.isoformat(), args.pages):
                entry = pool_entry(node, language, cutoff.isoformat(), excluded_names, excluded_repos)
                if entry:
                    pool[entry["name"].lower()] = entry
        args.out.write_text(json.dumps([pool[key] for key in sorted(pool)], indent=2) + "\n",
                            encoding="utf-8")
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")
    print(f"Saved {len(pool)} candidates.")


if __name__ == "__main__":
    main()
