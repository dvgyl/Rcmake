# SPDX-License-Identifier: MIT OR Apache-2.0
"""Shared reStructuredText include expansion for CMake Help generators."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
import re

INCLUDE = re.compile(r"^\.\. include:: +(\S+)[ \t]*$", re.MULTILINE)


def expand_includes(
    path: Path,
    help_root: Path,
    *,
    text: str | None = None,
    read_file: Callable[[Path], str] | None = None,
    max_depth: int = 16,
    strict: bool = True,
    prefix: str = "",
    _stack: tuple[Path, ...] = (),
) -> str:
    """Expand ``.. include::`` directives relative to a CMake Help root."""
    reader = read_file or (lambda target: target.read_text(encoding="utf-8"))
    if strict:
        current = path.resolve()
        root = help_root.resolve()
        if current in _stack or len(_stack) >= max_depth:
            raise ValueError(f"Cannot expand the include chain for {current}.")
        if not current.is_relative_to(root):
            raise ValueError(f"Include outside the Help tree: {current}")
    else:
        current = path
        root = help_root
        if len(_stack) >= max_depth:
            return reader(current) if text is None else text

    body = reader(current) if text is None else text
    next_stack = (*_stack, current)

    def include(match: re.Match[str]) -> str:
        target = match[1]
        included = (root / target.lstrip("/")) if target.startswith("/") else current.parent / target
        if not strict and not included.is_file():
            return ""
        return prefix + expand_includes(
            included,
            root,
            read_file=reader,
            max_depth=max_depth,
            strict=strict,
            prefix=prefix,
            _stack=next_stack,
        )

    return INCLUDE.sub(include, body)
