# reel-tui

> This project is 100% vibe coded using codex. I might rebuild in the future, I just wanted a tool quickly.

A terminal video inspector and track editor built with Rust, Ratatui, `ffprobe`,
and `ffmpeg`.

The sidebar lists every regular file in the current directory. Selecting a file
probes it in the background and displays a compact overview with separate
sections for every video, audio, subtitle, and other stream. Non-video files
remain visible and are identified in the details pane.

In the stream list, tracks can be marked and removed without re-encoding. The
file is remuxed to a temporary sibling and only replaces the original after the
result has been validated.

## Requirements

- A recent Rust toolchain
- `ffprobe` and `ffmpeg` available in `PATH`

## Run

Start the application from the directory you want to inspect:

```console
cargo run --release
```

After installing it, you can run `reel` directly:

```console
cargo install --path .
cd /path/to/videos
reel
```

## Keys

| Key | Action |
| --- | --- |
| `j`, `Down` | Select next file |
| `k`, `Up` | Select previous file |
| `gg`, `G` | Select first/last file or track |
| `Enter` | Select a file or open the selected stream's details |
| `Space` | Mark or unmark the selected track for deletion |
| `d` | Confirm deletion of all marked tracks |
| `Esc` | Return to the previous layer, or quit from the file list |
| `Ctrl-d`, `Ctrl-u` | Scroll details down/up |
| `r` | Rescan the directory |
| `q`, `Esc` | Quit |

At least one playable video track must remain. If a file has audio, at least one
audio track must also remain. All subtitle tracks may be removed.
