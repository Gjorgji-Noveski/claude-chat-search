# csearch

A fast CLI for fuzzy-searching every Claude Code conversation on your machine and jumping straight back into one.

## How it works

- Reads your local transcripts from `~/.claude/projects/`. Fully offline.
- Ranks conversations with fuzzy matching on titles and messages as you type.
- Press Enter to run `claude --resume <id>` in the folder where that conversation took place.

## Install

Requires [Rust](https://rustup.rs). Clone the repo and run the install script:

```sh
git clone https://github.com/Gjorgji-Noveski/claude-chat-search.git csearch && cd csearch
./install.sh
```

This builds a release binary and copies it to `~/.local/bin/csearch`. Use `BIN_DIR=/some/dir ./install.sh` to install somewhere else. If that folder isn't on your `PATH`, the script prints the line to add to your shell config. Re-run the script to update.

## Usage

```sh
csearch                  # interactive search
csearch some words       # start with a query
csearch --print words    # print top results, no UI
```
