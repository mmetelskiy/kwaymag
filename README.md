# kwaymag

A Wayland screen magnifier — replacement for [kmag](https://apps.kde.org/kmag/) on Wayland desktops.

Vibe coded in 2 evenings. Uses xdg-desktop-portal for screen capture via PipeWire and renders a magnified view in a floating window.

## Requirements

- Wayland compositor
- PipeWire
- xdg-desktop-portal (with a backend that supports screen capture, e.g. xdg-desktop-portal-gnome or xdg-desktop-portal-wlr)
- Rust toolchain

## Build

```sh
cargo build --release
```

The binary will be at `target/release/kwaymag`.

## Run

```sh
cargo run --release
```

Or run the compiled binary directly:

```sh
./target/release/kwaymag
```

On first launch, your desktop portal will prompt you to select a screen or window to capture. The selection is saved and reused on subsequent runs.

## Troubleshooting

**Wrong screen or window selected**

The portal restore token is cached so you are not prompted on every launch. To reset the selection, delete the token file and restart:

```sh
rm ~/.local/share/kwaymag/restore_token
```

If `XDG_DATA_HOME` is set, the file lives at `$XDG_DATA_HOME/kwaymag/restore_token` instead.
