# reel-tui

A read-only terminal video inspector built with Rust, Ratatui, and `ffprobe`.

The sidebar lists every regular file in the current directory. Selecting a file
probes it in the background and displays a compact overview with separate
sections for every video, audio, subtitle, and other stream. Non-video files
remain visible and are identified in the details pane.

## Requirements

- A recent Rust toolchain
- `ffprobe` available in `PATH` (normally provided by FFmpeg)

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
| `g`, `G` | Select first/last file |
| `Enter` | Select a file or open the selected stream's details |
| `Esc` | Return to the previous layer, or quit from the file list |
| `Ctrl-d`, `Ctrl-u` | Scroll details down/up |
| `r` | Rescan the directory |
| `q`, `Esc` | Quit |

This first version does not edit or play files. Its probe and application-state
layers are kept separate so future operations can invoke `ffmpeg` without
coupling editing logic to terminal rendering.
