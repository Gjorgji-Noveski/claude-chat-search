# csearch

A fast CLI for fuzzy-searching every Claude Code conversation on your machine and jumping straight back into one.

## How it works

- Reads your local transcripts from `~/.claude/projects/`. Fully offline.
- Ranks conversations with fuzzy matching on titles and messages as you type.
- Press Enter to run `claude --resume <id>` in the folder where that conversation took place.

```sh
cargo install --path .
csearch                  # interactive search
csearch some words       # start with a query
csearch --print words    # print top results, no UI
```
