# kwaymag

A Wayland screen magnifier — replacement for [kmag](https://apps.kde.org/kmag/) on Wayland desktops.

Tested on Kubuntu 26.04 LTS with Wayland

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

## What didn't work

- "follow the mouse" mode - wayland doesn't allow the app to get the mouse position
- avoid recursive portal - wayland doesn't allow the app to get it's position on the screen and pipewire has no api to exclude the window from the screencast
- hide the cursor while dragging - I was able to hide it but wasn't able to restore (though I think it's possible)

## Support

I vibecoded it in 3 evenings for personal use and have no plans to support the project

Feel free to fork and add the features you need
