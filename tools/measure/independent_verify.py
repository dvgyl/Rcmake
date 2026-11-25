#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Independent black-box differential verification CLI entrypoint."""

from __future__ import annotations

from pathlib import Path
import sys

if __package__:
    from .verify import *  # noqa: F403
    from .verify import __all__, main
else:
    _HERE = str(Path(__file__).resolve().parent)
    if _HERE not in sys.path:
        sys.path.insert(0, _HERE)
    from verify import *  # noqa: E402, F403
    from verify import __all__, main  # noqa: E402


if __name__ == "__main__":
    sys.exit(main())
