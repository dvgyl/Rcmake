<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Shell completions for rcmake

Completion scripts for `rcmake` cover configure options, native extensions (`explain`, `--diagnostics`, `--profile`, `--hermetic`, `--fetch-lock`, `--no-probe-cache`), `--build`, `--install`, `--workflow`, and `-E` subcommands.

## Bash

Install for your user account (or source directly in `~/.bashrc`):

```sh
mkdir -p ~/.local/share/bash-completion/completions && cp completions/rcmake.bash ~/.local/share/bash-completion/completions/rcmake
```

## Zsh

Copy `_rcmake` into a directory on `$fpath`, then reload completions with `compinit`:

```sh
mkdir -p ~/.zsh/completions && cp completions/_rcmake ~/.zsh/completions/_rcmake && echo 'fpath=(~/.zsh/completions $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc
```

## Fish

Install into the user completions directory:

```fish
mkdir -p ~/.config/fish/completions && cp completions/rcmake.fish ~/.config/fish/completions/rcmake.fish
```

## PowerShell

Dot-source `rcmake.ps1` from your PowerShell `$PROFILE`:

```powershell
New-Item -ItemType Directory -Force -Path (Split-Path $PROFILE) | Out-Null; Add-Content -Path $PROFILE -Value ". `"$((Resolve-Path completions/rcmake.ps1).Path)`""
```
