# reel 🎞️

> ❗ This project is 100% vibe coded using codex. I might rebuild in the future, I just wanted a tool quickly. ❗

A TUI to inspect and edit video files. Uses ffprobe and ffmpeg to inspect and edit media files.

## Requirements

- A recent Rust toolchain
- `ffprobe` and `ffmpeg` available in `PATH`

Optional subtitle tools:

- `seconv` for PGS/VobSub conversion, image subtitle rendering, and OCR
- `tesseract` plus installed language data for image-to-text OCR

Reel detects optional tools from `PATH`. Formats that cannot be handled by the
available tools or the current media container remain visible with a short
disabled reason.

## Installation

TODO

## Features

- Inspect container, duration, file size, bitrate, and chapter count
- Convert the container between MKV, MP4, MOV, and WebM while preserving
  compatible tracks; incompatible tracks are explained and must be resolved
  before saving
- View video, audio, subtitle, and other tracks grouped by type
- Reorder tracks, choose default tracks, and remove unwanted tracks
- Apply track edits without re-encoding by remuxing with `ffmpeg`
- Change individual video tracks to H.264, HEVC, or AV1 and downscale with presets
  or an exact custom resolution using padding, stretching, or aspect-ratio fitting
- Nest matching `movie.<language>.<format>` subtitle sidecars below their media
  file and show them beneath embedded tracks
- For embedded subtitles, choose a codec and independently check **Export
  sidecar**; a changed codec is used for both the container and exported file
- Convert sidecars between SRT, ASS, WebVTT, TTML, PGS, and VobSub when the
  required tools are available; sidecar-only changes do not remux the media
- OCR PGS/VobSub to text with `seconv` and Tesseract, or render text subtitles
  to image-based PGS/VobSub using the video resolution; OCR automatically uses
  matching installed language data, with English or the first available
  language as fallback
- Stage and validate media and subtitle outputs before publishing them together
- Replace the source by default after the new extension and container have been
  validated, or save container changes as a new file
