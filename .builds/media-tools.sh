#!/usr/bin/env bash
# Checks the external tools reel requires against the floors recorded in
# AGENTS.md, and installs seconv. FFmpeg, Tesseract, libicu and the ALSA headers
# come from the Arch image's packages (listed in each manifest); this fails
# loudly if any of them is below its floor rather than letting the suite run
# against something no user can.
set -euo pipefail

# FFmpeg 8.1+
ffmpeg -version | head -1
ffprobe -version | head -1
read -r major minor < <(ffprobe -version | head -1 | sed -E 's/^ffprobe version n?([0-9]+)\.([0-9]+).*/\1 \2/')
if (( major < 8 || (major == 8 && minor < 1) )); then
  echo "Resolved ffprobe is below the 8.1 floor" >&2
  exit 1
fi

# Tesseract 4.0+. libicu is seconv's, not Tesseract's: without it seconv aborts
# in Bootstrap.Initialize the moment it has to render a subtitle image.
tesseract --version | head -1

# The ALSA headers cpal builds against
# A build dependency, not a runtime one: `alsa-sys`'s build script is a bare
# `pkg_config::probe_library("alsa")`, so without these the crate fails to
# compile and nothing in the suite runs. The build VM has no sound device and
# does not need one — the timing page's playback falls back to
# `audio::SilentOutput`, which is the same production path a user without a
# sound card takes.
pkg-config --modversion alsa

# seconv 5.1.0+
# Pinned rather than tracking latest: seconv ships beta releases daily, and a
# release-time publish is the wrong place to discover one broke. SubtitleEdit
# publishes it only as a GitHub release, so this is the one download CI still
# makes from outside the image's package servers.
url="https://github.com/SubtitleEdit/subtitleedit/releases/download/v5.1.0/SeConv-Linux-x64.tar.gz"
sudo mkdir -p /opt/seconv
curl -fsSL --retry 5 --retry-all-errors "$url" | sudo tar -xz -C /opt/seconv
sudo chmod +x /opt/seconv/seconv
# A symlink, not a copy: seconv loads libSkiaSharp.so from beside its real
# path, and resolves that through the link.
sudo ln -sf /opt/seconv/seconv /usr/local/bin/seconv
# `--version` reports 5.0.0 even on 5.1.0, so it proves nothing beyond the
# binary running. The help listing is what distinguishes them.
seconv --version
seconv --help | grep -q -- --no-vobsub-isolate-colors \
  || { echo "seconv is older than 5.1.0" >&2; exit 1; }
