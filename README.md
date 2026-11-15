# Glotzkiste

A multi-stream live viewer: several Twitch streams and chats at once, in tabs and free tile layouts, with 7TV/BTTV/FFZ emotes, ultra-low latency and no ads for Turbo accounts. One Windows executable, nothing to install.

## Use

- **Add a stream:** type a channel name (or paste a twitch.tv link) into *Add channel…* and press Enter. Logged in, *Following* lists your live channels.
- **Arrange:** *Layout* applies Grid, Side by side, Spotlight or One-at-a-time, each with or without a chat column. Drag a tile by its top bar to move it, drag the gaps to resize. Double-click a tile (or ⛶) to maximize it in its tab; Esc goes back.
- **Sound:** click a stream to hear it; the one you hear has a purple frame.
- **Per stream** (hover the top of a tile): quality (Auto follows the tile size), volume, open its chat, remove.
- **Tabs:** Ctrl+T new, Ctrl+W close, Ctrl+1…9 or Ctrl+Tab switch, double-click to rename.
- **Latency:** *Ultra low* (default), *Low* or *Smooth* for shaky connections.
- **Stats:** F3 or *Stats*: frames shown, drops, decoder, buffer and network for every stream, laid out like your tiles.
- **Log in:** *Log in* walks you through it (an open-source browser extension, or the browser's own developer tools). The login is kept in Windows Credential Manager as `Glotzkiste/twitch`.

Everything you set up is saved and comes back the next time you start it.

`glotzkiste.exe --help` lists the command-line options (channels to open, `--mute`, `--chat`, …).

## Build

See `AGENTS.md` (build settings) and `docs/DESIGN.md` (architecture). `cargo build --release -p glotzkiste` with `.cargo/config.toml` from the template embeds libmpv; the result is `glotzkiste.exe`.
