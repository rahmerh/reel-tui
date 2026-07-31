# reel 🎞️

> ❗ This project is 100% vibe coded using AI. Code quality is probably lacking, but functionally well tested. ❗

A TUI to inspect and edit video files. Uses ffprobe and ffmpeg to inspect and edit media files.

## Requirements

- A recent Rust toolchain
- `ffprobe` and `ffmpeg` available in `PATH`

Optional subtitle tools:

- `seconv` for PGS/VobSub conversion, image subtitle rendering, and OCR
- `tesseract` plus installed language data for image-to-text OCR

Reel detects optional tools from `PATH`. When it can't find these tools you're not able to convert certain formats.

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

## Features

- **Media Inspection**: Inspect container formats, track layouts, duration, bitrates, and metadata across your video collection.
- **Container & Track Editing**: Convert container formats (MKV, MP4, MOV, WebM), reorder or remove audio/video/subtitle tracks, and set default streams without re-encoding.
- **Video Transcoding & Resizing**: Re-encode video streams to supported codecs (H.264, HEVC, AV1) and adjust resolutions with dynamic scaling and aspect-ratio fitting.
- **Subtitle Management**: Import, export, convert, and OCR subtitle tracks between text and image-based formats.
- **Network Share Support**: Work efficiently on local storage or remote network shares (NFS, SMB) with adaptive monitoring and metadata caching.

