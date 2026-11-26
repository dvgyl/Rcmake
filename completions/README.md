<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Shell completions for rcmake

These completion scripts cover `rcmake` configure options, `--build`, `--install`, `--workflow`, and `-E` subcommands. They also cover native rcmake extensions, including `explain`, `--diagnostics`, `--profile`, `--hermetic`, `--fetch-lock`, and `--no-probe-cache`.

## Bash

Install the completion script for your user account, or source `completions/rcmake.bash` directly from `~/.bashrc`.

```sh
mkdir -p ~/.local/share/bash-completion/completions && cp completions/rcmake.bash ~/.local/share/bash-completion/completions/rcmake
```

## Zsh

Copy `_rcmake` into a directory on `$fpath`, then reload completions with `compinit`.

```sh
mkdir -p ~/.zsh/completions && cp completions/_rcmake ~/.zsh/completions/_rcmake && echo 'fpath=(~/.zsh/completions $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc
```

## Fish

Copy `rcmake.fish` into your Fish completions directory.

```fish
mkdir -p ~/.config/fish/completions && cp completions/rcmake.fish ~/.config/fish/completions/rcmake.fish
```

## PowerShell

Dot-source `rcmake.ps1` from your PowerShell `$PROFILE`.

```powershell
New-Item -ItemType Directory -Force -Path (Split-Path $PROFILE) | Out-Null; Add-Content -Path $PROFILE -Value ". `"$((Resolve-Path completions/rcmake.ps1).Path)`""
```
