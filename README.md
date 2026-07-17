# reel 🎞️

> ❗ This project is 100% vibe coded using codex. I might rebuild in the future, I just wanted a tool quickly. ❗

A TUI to inspect and edit video files. Uses ffprobe and ffmpeg to inspect and edit media files.

## Requirements

- A recent Rust toolchain
- `ffprobe` and `ffmpeg` available in `PATH`

## Installation

TODO

## Features

- Inspect container, duration, file size, bitrate, and chapter count
- View video, audio, subtitle, and other tracks grouped by type
- Reorder tracks, choose default tracks, and remove unwanted tracks
- Apply track edits without re-encoding by remuxing with `ffmpeg`
- Change individual video tracks to H.264, HEVC, or AV1 and downscale their resolution
