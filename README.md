# votrim

Native video trimmer and encoder: a zoomable multi-segment timeline with a live
mpv preview, and ffmpeg presets for space-efficient output.

## Install

On Arch Linux, from the AUR:

```sh
paru -S votrim-bin
```

Otherwise take the tarball from the [latest release](https://github.com/dowoge/votrim/releases/latest).

## Requirements

- `ffmpeg` and `ffprobe` on `PATH` (built with `libsvtav1`, `libopus`, `libx264`,
  `libx265`, `libvpx-vp9` for the corresponding presets)
- `libmpv` (>= 2.0) for the preview
- A Rust toolchain

## Build and run

```sh
cargo build --release
./target/release/votrim [video-file]
```

## Editing

Mark segments with **I** (in) then **O** (out); every marked segment is kept and
the rest is dropped. Segments can also be dragged on the timeline: the lower bar
moves a segment, its ends resize it, and the upper strip scrubs.

| Key | Action |
| --- | --- |
| `Space` | Play / pause |
| `←` `→` | Step one frame |
| `Shift`+`←` `→` | Seek 1 s |
| `Ctrl`+`←` `→` | Seek 10 s |
| `I` / `O` | Mark in / mark out |
| `Delete` | Remove the selected segment |
| `Home` / `End` | Jump to start / end |
| `+` / `-` / `F` | Zoom in / out / fit |

Scroll over the timeline to zoom around the cursor, `Shift`-drag to pan, and drag
the overview strip at the bottom to move the visible window.

## Encoding

**Re-encode** cuts on the exact frame. **Stream copy** is instant but each cut
snaps back to the preceding keyframe; keyframe ticks appear on the timeline in
this mode and each segment shows where the cut will really land, with a button to
snap to it.

Rate control is CRF (optionally capped with SVT-AV1's `mbr`), a fixed bitrate, or
a target file size, which solves for the bitrate over the total selected duration
and runs two passes. The default preset is the space-efficient AV1 configuration:

```sh
ffmpeg -i input.mp4 -c:v libsvtav1 -crf 48 -preset 7 \
  -svtav1-params lookahead=120:tune=0:mbr=10m -c:a libopus output.mp4
```

Presets are saved to `~/.config/votrim/presets.json`. Tick **show command** to see
the exact ffmpeg invocations before running them.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
