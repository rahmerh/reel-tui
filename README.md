# reel 🎞️

> ❗ This project is 100% vibe coded using AI. I just wanted a tool quickly, I'll probably rewrite this myself in the future. ❗

A TUI for inspecting, editing, and converting media files. Uses ffprobe and ffmpeg to inspect
and edit media files.

## Requirements

- A recent Rust toolchain
- `ffprobe` and `ffmpeg` available in `PATH`

Optional subtitle tools:

- `seconv` for PGS/VobSub conversion, image subtitle rendering, and OCR
- `tesseract` plus installed language data for image-to-text OCR

Reel detects optional tools from `PATH`, if not found you can still use the rest of the app.

Desktop completion notifications use `notify-send` when it is available.

## Installation

```sh
cargo install reel-tui
```

## Usage

Launch `reel` in the directory containing your media files:

```sh
reel
```

Or pass a target directory path:

```sh
reel /path/to/media
```

Desktop notifications are enabled by default. Disable them in
`~/.config/reel/config.toml`:

```toml
[notifications]
enabled = false
```

Use `reel --help` (or `reel -h`) to show command-line help, and `reel --version`
(or `reel -V`) to print the installed version.

## How It Works

`reel` uses a non-destructive, queued editing model:

1. **Queue Edits**: As you navigate files and adjust container formats, track orders, video or audio encoding settings, or subtitle sidecars, your changes are held in an in-memory queue without modifying any files on disk.
2. **Review & Execute (`Ctrl+S`)**: Pressing `Ctrl+S` opens a confirmation dialog that summarizes all queued operations for validation.
3. **Batch Processing**: Once confirmed, `reel` executes the queued operations in a single pass via `ffmpeg`—staging intermediate work in scratch storage before publishing the final file.

## Features

- **Media Inspection**: Inspect container formats, track layouts, duration, bitrates, and metadata across your video collection.
- **Container & Track Editing**: Convert container formats (MKV, MP4, MOV, WebM), reorder or remove audio/video/subtitle tracks, and modify stream metadata without re-encoding.
- **Video Transcoding & Resizing**: Re-encode video streams to supported codecs (H.264, HEVC, AV1) and adjust resolutions with dynamic scaling and aspect-ratio fitting.
- **Audio Track Editing**: Convert individual audio tracks to AAC, AC-3, E-AC-3, Opus, FLAC, ALAC, MP3, or Vorbis; downmix channels; and edit language, title, default, and role metadata. Reel automatically selects safe bitrate and sample-rate values for the chosen codec.
- **Subtitle Management**: Import, export, convert, and OCR subtitle tracks between text and image-based formats. Edit metadata for both embedded subtitle tracks and external sidecars.
- **Network Share Support**: Work efficiently on local storage or remote network shares (NFS, SMB) with adaptive monitoring and metadata caching.

## Future planned

- **Subtitle editing**: More in depth editing of subtitles, timing, text, style and hopefully more.
- **Video track metadata editing**: Edit metadata attached to individual video tracks.
