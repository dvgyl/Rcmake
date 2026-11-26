#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: install.sh [--version <v> | --from-source | --archive <path>] [--prefix <dir>] [--alias-cmake] [--force]
       install.sh --uninstall [--prefix <dir>]

Install rcmake and rcmake-lsp into $PREFIX/bin, the bundled CMake 4.5.1
data into $PREFIX/share/cmake-4.5, licences and third-party notices into
$PREFIX/share/doc/rcmake, and any bundled shell completions and man page
under $PREFIX/share. The default PREFIX is $HOME/.local.

Sources (select one, the default is the latest GitHub Release):
  --version <v>      Download GitHub Release <v>, for example v0.1.0 or 0.1.0.
  --from-source      Build a release archive from this checkout (needs Cargo).
  --archive <path>   Install a local .tar.gz archive. The script checks
                     <path>.sha256 if that file exists.

Options:
  --prefix <dir>     Set the installation root (default: $PREFIX or $HOME/.local).
  --alias-cmake      Also create cmake, ctest, and cpack links to rcmake.
  --force            Allow replacement of unmarked share/cmake-4.5 data.
                     With --alias-cmake, also replace existing aliases.
  --uninstall        Remove rcmake, rcmake-lsp, the links to rcmake,
                     managed share/cmake-4.5 data, share/doc/rcmake, and the rcmake
                     completions and man page from $PREFIX.
  -h, --help         Show this help.

Requirements: bash, python3, and curl (for release downloads).
EOF
}

die() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

mode=release
archive=""
version=""
prefix="${PREFIX:-$HOME/.local}"
alias_cmake=false
force=false
uninstall=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --from-source|--archive)
      if [[ "$mode" != release ]]; then
        printf 'Select one install source.\n' >&2
        exit 2
      fi
      if [[ "$1" == --archive ]]; then
        if [[ $# -lt 2 || -z "$2" || "$2" == --* ]]; then
          printf '%s requires a path.\n' "$1" >&2
          exit 2
        fi
        mode=archive
        archive="$2"
        shift
      else
        mode=source
      fi
      ;;
    --version)
      if [[ $# -lt 2 || -z "$2" || "$2" == --* ]]; then
        printf '%s requires a version.\n' "$1" >&2
        exit 2
      fi
      version="$2"
      shift
      ;;
    --prefix)
      if [[ $# -lt 2 || -z "$2" || "$2" == --* ]]; then
        printf '%s requires a path.\n' "$1" >&2
        exit 2
      fi
      prefix="$2"
      shift
      ;;
    --alias-cmake) alias_cmake=true ;;
    --force) force=true ;;
    --uninstall) uninstall=true ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown option %s. Run install.sh --help.\n' "$1" >&2; exit 2 ;;
  esac
  shift
done

if [[ -n "$version" ]]; then
  if [[ "$mode" != release || "$uninstall" == true ]]; then
    printf 'Use --version only to download a release.\n' >&2
    exit 2
  fi
  if [[ ! "${version#v}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.+-]+)?$ ]]; then
    printf 'Invalid version %s. Use a release version such as v0.1.0.\n' "$version" >&2
    exit 2
  fi
  version="${version#v}"
fi

bin_dir="$prefix/bin"
data_dir="$prefix/share/cmake-4.5"
data_marker="$data_dir/.rcmake-managed"
doc_dir="$prefix/share/doc/rcmake"
tools=(cmake ctest cpack)
extra_share_files=(
  "$prefix/share/man/man1/rcmake.1"
  "$prefix/share/bash-completion/completions/rcmake"
  "$prefix/share/zsh/site-functions/_rcmake"
  "$prefix/share/fish/vendor_completions.d/rcmake.fish"
  "$prefix/share/rcmake/completions/rcmake.ps1"
)

is_rcmake_link() {
  [[ -L "$1" && "$(readlink "$1")" == rcmake ]]
}

if "$uninstall"; then
  if [[ "$mode" != release || "$alias_cmake" == true || "$force" == true ]]; then
    printf 'Use --uninstall with only an optional --prefix.\n' >&2
    exit 2
  fi
  removed=()
  for tool in "${tools[@]}"; do
    if is_rcmake_link "$bin_dir/$tool"; then
      rm -f "$bin_dir/$tool"
      removed+=("$bin_dir/$tool")
    fi
  done
  if [[ -f "$bin_dir/rcmake" || -L "$bin_dir/rcmake" ]]; then
    rm -f "$bin_dir/rcmake"
    removed+=("$bin_dir/rcmake")
  fi
  if [[ -f "$bin_dir/rcmake-lsp" && ! -L "$bin_dir/rcmake-lsp" ]] || is_rcmake_link "$bin_dir/rcmake-lsp"; then
    rm -f "$bin_dir/rcmake-lsp"
    removed+=("$bin_dir/rcmake-lsp")
  fi
  if [[ -d "$data_dir" ]]; then
    if [[ -L "$data_dir" || ! -f "$data_marker" || -L "$data_marker" ]]; then
      printf 'Kept %s because it has no rcmake ownership marker.\n' "$data_dir"
    elif [[ -e "$bin_dir/cmake" ]] && ! is_rcmake_link "$bin_dir/cmake"; then
      printf 'Kept %s because %s is another CMake installation.\n' "$data_dir" "$bin_dir/cmake"
    else
      rm -rf "$data_dir"
      removed+=("$data_dir")
    fi
  fi
  doc_manifest="$doc_dir/.rcmake-manifest"
  if [[ -f "$doc_manifest" && ! -L "$doc_manifest" ]]; then
    while IFS= read -r rel || [[ -n "$rel" ]]; do
      [[ -z "$rel" ]] && continue
      case "$rel" in
        /*|..|../*|*/..|*/../*|.) continue ;;
      esac
      target="$doc_dir/$rel"
      if [[ -f "$target" && ! -L "$target" ]]; then
        rm -f "$target"
        removed+=("$target")
        parent="$(dirname "$target")"
        while [[ "$parent" != "$doc_dir" && "$parent" == "$doc_dir/"* ]]; do
          rmdir "$parent" 2>/dev/null || break
          parent="$(dirname "$parent")"
        done
      fi
    done < "$doc_manifest"
    rm -f "$doc_manifest"
  elif [[ -d "$doc_dir" && ! -L "$doc_dir" ]]; then
    for notice in LICENSE-APACHE LICENSE-MIT THIRD_PARTY_NOTICES.md; do
      target="$doc_dir/$notice"
      if [[ -f "$target" && ! -L "$target" ]]; then
        rm -f "$target"
        removed+=("$target")
      fi
    done
  fi
  if [[ -d "$doc_dir" && ! -L "$doc_dir" ]]; then
    rmdir "$doc_dir/third_party/licences" 2>/dev/null || true
    rmdir "$doc_dir/third_party" 2>/dev/null || true
    rmdir "$doc_dir" 2>/dev/null || true
    rmdir "$prefix/share/doc" 2>/dev/null || true
  fi
  for extra in "${extra_share_files[@]}"; do
    if [[ -f "$extra" && ! -L "$extra" ]]; then
      rm -f "$extra"
      removed+=("$extra")
      if [[ "$extra" == "$prefix/share/rcmake/completions/rcmake.ps1" ]]; then
        rmdir "$prefix/share/rcmake/completions" 2>/dev/null || true
        rmdir "$prefix/share/rcmake" 2>/dev/null || true
      fi
    fi
  done
  if [[ ${#removed[@]} -eq 0 ]]; then
    printf 'Found no rcmake installation in %s.\n' "$prefix"
  else
    printf 'Removed:\n'
    printf '  %s\n' "${removed[@]}"
  fi
  case "$(uname -s)" in
    Darwin) cache="$HOME/Library/Caches/rcmake" ;;
    *) cache="${XDG_CACHE_HOME:-$HOME/.cache}/rcmake" ;;
  esac
  if [[ -d "${RCMAKE_CACHE_DIR:-$cache}" ]]; then
    printf 'The probe and fetch cache stays in %s. Delete it to free disk space.\n' "${RCMAKE_CACHE_DIR:-$cache}"
  fi
  exit 0
fi

command -v python3 >/dev/null 2>&1 || die 'python3 is required.'

# Find alias collisions before the script downloads or writes anything.
# A partial alias set would run a mix of rcmake and another CMake.
if "$alias_cmake"; then
  blocked=()
  for tool in "${tools[@]}"; do
    alias="$bin_dir/$tool"
    if is_rcmake_link "$alias" || [[ ! -e "$alias" && ! -L "$alias" ]]; then
      continue
    fi
    if [[ -d "$alias" && ! -L "$alias" ]]; then
      die "$alias is a directory. Remove it, then run the installer again."
    fi
    blocked+=("$alias")
  done
  if [[ ${#blocked[@]} -gt 0 ]] && ! "$force"; then
    printf 'install.sh: These files exist and are not links to rcmake:\n' >&2
    printf '  %s\n' "${blocked[@]}" >&2
    printf 'Run again with --force to replace them, or omit --alias-cmake.\n' >&2
    printf 'The installer changed nothing.\n' >&2
    exit 1
  fi
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/rcmake-install.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
checksum=""
case "$mode" in
  source)
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
    printf 'Build a release archive from %s.\n' "$root"
    "$root/scripts/dist.sh" --out-dir "$work"
    archives=("$work"/*.tar.gz)
    archive="${archives[0]}"
    checksum="$archive.sha256"
    ;;
  archive)
    [[ -f "$archive" ]] || die "$archive does not exist."
    if [[ -f "$archive.sha256" ]]; then
      checksum="$archive.sha256"
    else
      printf 'Warning: %s.sha256 does not exist. The installer cannot verify the SHA-256 checksum of this archive.\n' "$archive" >&2
    fi
    ;;
  release)
    command -v curl >/dev/null 2>&1 || die 'curl is required to download a release.'
    case "$(uname -s):$(uname -m)" in
      Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
      Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
      Darwin:arm64) target=aarch64-apple-darwin ;;
      *) die 'No release archive for this host. Use --from-source in a checkout on Linux or macOS.' ;;
    esac
    releases=https://github.com/dvgyl/rcmake/releases
    suffix="-$target.tar.gz"
    if [[ -n "$version" ]]; then
      downloads="$releases/download/v$version"
      expected="rcmake-$version$suffix"
      printf 'Download the checksums of release v%s.\n' "$version"
    else
      downloads="$releases/latest/download"
      expected=""
      printf 'Download the latest release checksums.\n'
    fi
    checksum="$work/SHA256SUMS.txt"
    if ! curl -fsSL --retry 3 --connect-timeout 20 --max-time 600 \
      "$downloads/SHA256SUMS.txt" -o "$checksum"; then
      die "Could not download $downloads/SHA256SUMS.txt. Check the version, or clone the repository and run scripts/install.sh --from-source."
    fi
    name="$(awk -v suffix="$suffix" -v expected="$expected" '
      substr($2, length($2) - length(suffix) + 1) == suffix && (expected == "" || $2 == expected) { print $2 }
    ' "$checksum")"
    if [[ ! "$name" =~ ^rcmake-[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.+-]+)?-${target}\.tar\.gz$ ]]; then
      if [[ -n "$version" ]]; then
        die "Release v$version has no archive for $target."
      fi
      die "No unique release archive for $target."
    fi
    archive="$work/$name"
    printf 'Download %s.\n' "$name"
    curl -fSL --retry 3 --connect-timeout 20 --max-time 600 --progress-bar \
      "$downloads/$name" -o "$archive"
    ;;
esac

python3 - "$archive" "$checksum" "$prefix" "$alias_cmake" "$force" <<'PY'
from __future__ import annotations

import hashlib
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

archive, checksum, prefix, aliases, force = sys.argv[1:]
archive, prefix = Path(archive), Path(prefix)
tools = ("cmake", "ctest", "cpack")
data_dir = Path("share/cmake-4.5")
data_marker = Path(".rcmake-managed")
doc_dir = Path("share/doc/rcmake")
doc_files = (
    Path("LICENSE-APACHE"),
    Path("LICENSE-MIT"),
    Path("THIRD_PARTY_NOTICES.md"),
)
licences_dir = Path("third_party/licences")
rcmake_rel = Path("bin/rcmake")
lsp_rel = Path("bin/rcmake-lsp")
binaries = (rcmake_rel, lsp_rel)
extra_share_files = (
    Path("share/man/man1/rcmake.1"),
    Path("share/bash-completion/completions/rcmake"),
    Path("share/zsh/site-functions/_rcmake"),
    Path("share/fish/vendor_completions.d/rcmake.fish"),
    Path("share/rcmake/completions/rcmake.ps1"),
)

try:
    if checksum:
        lines = Path(checksum).read_text(encoding="ascii").splitlines()
        expected = [fields[0] for line in lines if len(fields := line.split(maxsplit=1)) == 2
                    and fields[1].lstrip("*") == archive.name]
        if expected != [hashlib.sha256(archive.read_bytes()).hexdigest()]:
            release = re.fullmatch(
                r"rcmake-(\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.+-]+)?)-"
                r"(?:x86_64|aarch64)-(?:apple-darwin|unknown-linux-gnu)\.tar\.gz",
                archive.name,
            )
            label = f"release v{release[1]}, archive {archive.name}" if release else f"archive {archive.name}"
            raise ValueError(f"SHA-256 mismatch for {label}.")
        print(f"Verified the SHA-256 checksum of {archive.name}.")

    prefix.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=prefix, prefix=".rcmake-install-") as temporary:
        work = Path(temporary)
        top = None
        with tarfile.open(archive, "r:gz") as tar:
            for member in tar:
                path = PurePosixPath(member.name)
                if path.is_absolute() or ".." in path.parts or not path.parts:
                    raise ValueError(f"Invalid archive path {member.name!r}.")
                if top is None:
                    top = path.parts[0]
                if path.parts[0] != top:
                    raise ValueError("The archive must have one top-level directory.")
                relative = Path(*path.parts[1:])
                if relative in binaries:
                    output = work / relative
                    if member.isfile():
                        output.parent.mkdir(parents=True, exist_ok=True)
                        with tar.extractfile(member) as source, output.open("xb") as destination:
                            shutil.copyfileobj(source, destination)
                        output.chmod(0o755 if member.mode & 0o111 else 0o644)
                    elif relative == lsp_rel and member.issym():
                        link = PurePosixPath(member.linkname)
                        if (
                            not member.linkname
                            or member.linkname.endswith("/")
                            or link.is_absolute()
                            or ".." in link.parts
                            or link.parts != ("rcmake",)
                        ):
                            raise ValueError(f"Invalid symlink target {member.linkname!r} for {member.name!r}.")
                        output.parent.mkdir(parents=True, exist_ok=True)
                        output.symlink_to("rcmake")
                    else:
                        raise ValueError(f"Invalid archive entry {member.name!r}.")
                elif relative.is_relative_to(data_dir):
                    output = work / relative
                    if member.isdir():
                        output.mkdir(parents=True, exist_ok=True)
                    elif member.isfile():
                        output.parent.mkdir(parents=True, exist_ok=True)
                        with tar.extractfile(member) as source, output.open("xb") as destination:
                            shutil.copyfileobj(source, destination)
                        output.chmod(0o755 if member.mode & 0o111 else 0o644)
                    else:
                        raise ValueError(f"Invalid archive entry {member.name!r}.")
                elif relative in extra_share_files:
                    if not member.isfile():
                        raise ValueError(f"Invalid archive entry {member.name!r}.")
                    output = work / relative
                    output.parent.mkdir(parents=True, exist_ok=True)
                    with tar.extractfile(member) as source, output.open("xb") as destination:
                        shutil.copyfileobj(source, destination)
                    output.chmod(0o644)
                elif relative in doc_files:
                    if not member.isfile():
                        raise ValueError(f"Invalid archive entry {member.name!r}.")
                    output = work / doc_dir / relative
                    output.parent.mkdir(parents=True, exist_ok=True)
                    with tar.extractfile(member) as source, output.open("xb") as destination:
                        shutil.copyfileobj(source, destination)
                    output.chmod(0o644)
                elif relative.is_relative_to(licences_dir):
                    output = work / doc_dir / relative
                    if member.isdir():
                        output.mkdir(parents=True, exist_ok=True)
                    elif member.isfile():
                        output.parent.mkdir(parents=True, exist_ok=True)
                        with tar.extractfile(member) as source, output.open("xb") as destination:
                            shutil.copyfileobj(source, destination)
                        output.chmod(0o644)
                    else:
                        raise ValueError(f"Invalid archive entry {member.name!r}.")

        staged_bin = work / rcmake_rel
        if staged_bin.is_symlink() or not staged_bin.is_file():
            raise ValueError("Missing bin/rcmake in the archive.")
        staged_bin.chmod(0o755)
        staged_lsp = work / lsp_rel
        staged_lsp_is_link = staged_lsp.is_symlink()
        staged_lsp_is_file = staged_lsp.is_file() and not staged_lsp_is_link
        if staged_lsp_is_file:
            staged_lsp.chmod(0o755)
        for required in ("Modules/CMake.cmake", "Templates/CPackConfig.cmake.in"):
            if not (work / data_dir / required).is_file():
                raise ValueError(f"Missing {required} in the archive.")
        (work / data_dir / data_marker).write_text("Managed by rcmake.\n", encoding="ascii")
        subprocess.run([staged_bin, "--version"], check=True, stdout=subprocess.DEVNULL, timeout=30)

        staged_extras = [rel for rel in extra_share_files if (work / rel).is_file()]
        staged_doc = work / doc_dir
        staged_doc_files = (
            sorted(path.relative_to(staged_doc) for path in staged_doc.rglob("*") if path.is_file())
            if staged_doc.is_dir()
            else []
        )
        has_docs = bool(staged_doc_files)
        installed_doc = prefix / doc_dir
        doc_manifest_rel = Path(".rcmake-manifest")
        doc_manifest_path = installed_doc / doc_manifest_rel
        bin_dir = prefix / "bin"
        installed_data = prefix / data_dir

        def check_ancestor_dirs(target_path: Path, *, reject_symlinks_under: Path | None = None) -> None:
            ancestors: list[Path] = []
            cur = target_path.parent
            while cur != prefix and cur != cur.parent:
                ancestors.append(cur)
                cur = cur.parent
            for directory in reversed(ancestors):
                if not os.path.lexists(directory):
                    continue
                if (
                    reject_symlinks_under is not None
                    and directory.is_relative_to(reject_symlinks_under)
                    and directory.is_symlink()
                ):
                    raise ValueError(
                        f"{directory} is a symlink. Remove it, then run the installer again."
                    )
                if not directory.is_dir():
                    raise ValueError(
                        f"{directory} exists and is not a directory. Remove it, then run the installer again."
                    )

        check_ancestor_dirs(bin_dir / "rcmake")
        check_ancestor_dirs(installed_data)
        marker = installed_data / data_marker
        if os.path.lexists(installed_data) and force != "true":
            if installed_data.is_symlink() or not marker.is_file() or marker.is_symlink():
                raise ValueError(
                    f"{installed_data} has no rcmake ownership marker. Another CMake installation may own it. "
                    "Use --prefix <separate-dir> or --force to replace the data."
                )
        for target_name, present in (("rcmake", True), ("rcmake-lsp", staged_lsp_is_link or staged_lsp_is_file)):
            if present:
                target_path = bin_dir / target_name
                if target_path.is_dir() and not target_path.is_symlink():
                    raise ValueError(f"{target_path} is a directory. Remove it, then run the installer again.")
        if aliases == "true":
            for tool in tools:
                alias = bin_dir / tool
                if not (alias.is_symlink() and os.readlink(alias) == "rcmake"):
                    if os.path.lexists(alias) and (force != "true" or (alias.is_dir() and not alias.is_symlink())):
                        raise ValueError(f"{alias} exists. Run again with --force to replace it.")
        for rel in staged_extras:
            target_path = prefix / rel
            check_ancestor_dirs(target_path)
            if target_path.is_dir() and not target_path.is_symlink():
                raise ValueError(f"{target_path} is a directory. Remove it, then run the installer again.")
        if has_docs:
            check_ancestor_dirs(doc_manifest_path, reject_symlinks_under=installed_doc)
            if os.path.lexists(doc_manifest_path) and (
                doc_manifest_path.is_dir() or doc_manifest_path.is_symlink()
            ):
                raise ValueError(
                    f"{doc_manifest_path} is a directory or symlink. Remove it, then run the installer again."
                )
            for rel in staged_doc_files:
                target_path = installed_doc / rel
                check_ancestor_dirs(target_path, reject_symlinks_under=installed_doc)
                if os.path.lexists(target_path) and (target_path.is_dir() or target_path.is_symlink()):
                    raise ValueError(
                        f"{target_path} is a directory or symlink. Remove it, then run the installer again."
                    )

        rollback_dir = work / "rollback"
        rollback_dir.mkdir()
        backups: list[tuple[Path, Path | None]] = []
        created_dirs: list[Path] = []

        def ensure_dir(directory: Path) -> None:
            missing: list[Path] = []
            cur = directory
            while cur != prefix and cur != cur.parent and not cur.exists():
                missing.append(cur)
                cur = cur.parent
            for path in reversed(missing):
                path.mkdir()
                created_dirs.append(path)

        def record_and_backup(destination: Path) -> None:
            if os.path.lexists(destination):
                saved = rollback_dir / str(len(backups))
                destination.replace(saved)
                backups.append((destination, saved))
            else:
                backups.append((destination, None))

        def read_doc_manifest(path: Path) -> list[Path]:
            if not (path.is_file() and not path.is_symlink()):
                return []
            owned: list[Path] = []
            for raw_line in path.read_text(encoding="utf-8", errors="replace").splitlines():
                line = raw_line.strip()
                if not line:
                    continue
                posix = PurePosixPath(line)
                if posix.is_absolute() or ".." in posix.parts or "." in posix.parts or not posix.parts:
                    continue
                owned.append(Path(*posix.parts))
            return owned

        installed: list[object] = []
        try:
            ensure_dir(bin_dir)
            ensure_dir(installed_data.parent)
            record_and_backup(installed_data)
            (work / data_dir).replace(installed_data)

            record_and_backup(bin_dir / "rcmake")
            staged_bin.replace(bin_dir / "rcmake")
            installed.append(bin_dir / "rcmake")

            if staged_lsp_is_link:
                record_and_backup(bin_dir / "rcmake-lsp")
                staged_lsp.replace(bin_dir / "rcmake-lsp")
                installed.append(f"{bin_dir / 'rcmake-lsp'} -> rcmake")
            elif staged_lsp_is_file:
                record_and_backup(bin_dir / "rcmake-lsp")
                staged_lsp.replace(bin_dir / "rcmake-lsp")
                installed.append(bin_dir / "rcmake-lsp")

            if aliases == "true":
                for tool in tools:
                    alias = bin_dir / tool
                    if not (alias.is_symlink() and os.readlink(alias) == "rcmake"):
                        record_and_backup(alias)
                        new_alias = work / tool
                        new_alias.symlink_to("rcmake")
                        new_alias.replace(alias)
                    installed.append(f"{alias} -> rcmake")
            installed.append(installed_data)

            if has_docs:
                previously_owned = read_doc_manifest(doc_manifest_path)
                new_doc_set = {rel.as_posix() for rel in staged_doc_files}
                ensure_dir(installed_doc)
                for rel in staged_doc_files:
                    destination = installed_doc / rel
                    ensure_dir(destination.parent)
                    record_and_backup(destination)
                    (staged_doc / rel).replace(destination)
                    installed.append(destination)
                for old_rel in previously_owned:
                    if old_rel.as_posix() in new_doc_set:
                        continue
                    old_path = installed_doc / old_rel
                    if old_path.is_file() and not old_path.is_symlink():
                        record_and_backup(old_path)
                        parent = old_path.parent
                        while parent != installed_doc and parent.is_relative_to(installed_doc):
                            try:
                                parent.rmdir()
                            except OSError:
                                break
                            parent = parent.parent
                staged_manifest = work / "new-doc-manifest"
                staged_manifest.write_text(
                    "\n".join(sorted(new_doc_set)) + "\n",
                    encoding="utf-8",
                )
                staged_manifest.chmod(0o644)
                record_and_backup(doc_manifest_path)
                staged_manifest.replace(doc_manifest_path)

            for rel in staged_extras:
                destination = prefix / rel
                ensure_dir(destination.parent)
                record_and_backup(destination)
                (work / rel).replace(destination)
                installed.append(destination)

            version = subprocess.run([bin_dir.resolve() / "rcmake", "--version"], check=True, timeout=30,
                                     stdout=subprocess.PIPE, text=True).stdout.splitlines()
        except Exception:
            for destination, saved in reversed(backups):
                if os.path.lexists(destination):
                    if destination.is_dir() and not destination.is_symlink():
                        shutil.rmtree(destination)
                    else:
                        destination.unlink()
                if saved is not None and os.path.lexists(saved):
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    saved.replace(destination)
            for directory in reversed(created_dirs):
                try:
                    directory.rmdir()
                except OSError:
                    pass
            raise

        name = next((line for line in version if line.lower().startswith("rcmake ")), "rcmake")
        print(f"Installed {name.lower()} into {prefix}:")
        for item in installed:
            print(f"  {item}")
except (OSError, ValueError, tarfile.TarError, subprocess.SubprocessError) as error:
    sys.exit(f"Install failed. {error}")
PY

bin_real="$(cd "$bin_dir" && pwd -P)"
on_path=false
IFS=: read -r -a path_entries <<< "${PATH:-}"
for entry in ${path_entries[@]+"${path_entries[@]}"}; do
  if [[ -n "$entry" && -d "$entry" && "$(cd "$entry" && pwd -P)" == "$bin_real" ]]; then
    on_path=true
    break
  fi
done
if ! "$on_path"; then
  printf '\n%s is not on PATH. Add this line to your shell profile (for example ~/.bashrc or ~/.zshrc):\n' "$bin_dir"
  printf '  export PATH="%s:$PATH"\n' "$bin_dir"
elif "$alias_cmake"; then
  first="$(command -v cmake || true)"
  if [[ -n "$first" && "$(cd "$(dirname "$first")" && pwd -P)" != "$bin_real" ]]; then
    printf '\n%s comes before %s on PATH. Put %s first to use rcmake as cmake.\n' "$first" "$bin_dir/cmake" "$bin_dir"
  fi
fi

printf 'Run rcmake --version to check the installation.\n'
