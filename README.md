# decklink-desktop

Play out the X11 desktop on a Blackmagic DeckLink card (tested with an
Intensity Pro 4K), either the whole desktop or a single RandR output. You
can make that output a virtual monitor, arrange it with `xrandr` or
`arandr`, and move windows onto it.

Built on the [`decklink`](https://github.com/simukka/decklink) crate.

## How it works

- The screen is captured with MIT-SHM `GetImage`, and the cursor is
  composited on top via XFixes, since the hardware cursor isn't part of the
  captured image.
- Capture runs on its own thread with double-buffered shared memory, so a
  slow framebuffer readback overlaps with scaling and playout.
- Frames are scaled to the DeckLink mode with a multithreaded fixed-point
  bilinear scaler, or copied 1:1 when the sizes match, then sent as 8-bit
  BGRA using the card's scheduled playback.

## Usage

```sh
# Mirror the whole desktop, scaled into 1080p60
decklink-desktop

# Follow one RandR output/monitor and play out only its area
decklink-desktop --output Virtual-1-1
```

| Option | Default | |
|---|---|---|
| `--output NAME` | whole desktop | RandR monitor to follow |
| `--mode MODE` | match the output (1080p60 for the whole desktop) | `1080p2398` … `1080p60`, `2160p2398` … `2160p30` |
| `--scale ALGO` | multithreaded bilinear | the `decklink` crate's scalers: `nearest`, `bilinear`, `bicubic`, `lanczos`, `lanczos-gpu` |
| `--device N` | `0` | DeckLink device index |

With `--output`, the tool polls RandR twice a second:

- **Output enabled:** playout starts with the DeckLink mode that matches the
  output's resolution and refresh rate (e.g. 1920x1080 at 59.94 Hz uses
  `1080p5994`).
- **Mode or position changed:** the card is reconfigured.
- **Output disabled:** playout stops until the output is enabled again.

Other sizes are scaled into 1080p60.

## A virtual monitor for the card

A DeckLink card is not a GPU output, so it never shows up in RandR by
itself. The kernel's `vkms` driver adds a virtual connector, and X can drive
it as a PRIME output of the real GPU:

```sh
./virtual-output.sh            # sudo modprobe vkms + xrandr --setprovideroutputsource
xrandr --output Virtual-1-1 --mode 1920x1080 --right-of DisplayPort-3   # or use arandr
decklink-desktop --output Virtual-1-1
```

In i3 the new output gets its own workspace. To move a window there, run
`i3-msg move container to output Virtual-1-1`.

A region made with `xrandr --setmonitor` also works as an `--output`. It
needs no kernel module, but `arandr` doesn't show it.

## Building

Build requirements:

- The DeckLink SDK (Linux) headers, with `DECKLINK_SDK_PATH` pointing at the
  SDK's `Linux/include` directory.
- libclang, used by bindgen.

At runtime, Blackmagic Desktop Video must be installed. See the
[`decklink` crate](https://github.com/simukka/decklink#building) for details.

```sh
DECKLINK_SDK_PATH=/path/to/SDK/Linux/include cargo build --release
```

To avoid exporting it every time, set it in `.cargo/config.toml`:

```toml
[env]
DECKLINK_SDK_PATH = "/path/to/SDK/Linux/include"
```

The `gpu` feature is on by default and enables `--scale lanczos-gpu`. Build
with `--no-default-features` to skip wgpu.

## License

LGPL-2.1-or-later, like the `decklink` crate.
