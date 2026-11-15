# Changelog

## Unreleased

- **Every GPU:** streams decode on the other GPUs first (integrated ones too) and on the GPU drawing the window last; a full decoder hands streams on to the next GPU with room, the CPU last. Frames of streams on another GPU are copied over at tile size without waiting for that GPU (0.2 ms per frame measured). Right-click a stream to choose where it decodes; the stats show which GPU decodes each stream.
- Auto quality steps down per GPU, only where that decoder is busy.
- **Stats show what holds each stream back:** network, the broadcast itself (fewer frames than its variant says), the decoder (with that GPU's load), the GPU copy or the display, with the numbers behind it, and a count over the tab. The table fills the window with fixed columns that no longer jump.

## 1.0.0

The first release.

- **Many streams at once:** tabs of free tile layouts. Grid, side by side, spotlight and one-at-a-time presets, with or without a chat column; drag tiles by their top bar, drag the gaps to resize, double-click to maximize.
- **Ultra-low latency:** its own live feeder starts at the segment being encoded and streams it into the player as it arrives; latency modes Ultra low, Low and Smooth keep the delay steady by speeding up or easing off slightly.
- **Fast:** hardware decoding straight to the screen without copies, one render thread for every stream with each frame drawn at its due time, the interface redrawn only when something changes. Auto quality follows the tile size and keeps 60 fps.
- **Chat:** live chat with 7TV, BTTV and FFZ emotes (animated), badges, mentions and links; switch a chat to any channel, or combine several (or all streams in the tab) into one.
- **No ads with Turbo:** log in with your browser session, guided step by step (open-source cookie exporter or the browser's developer tools); the login is kept in Windows Credential Manager. Without Turbo, ad breaks are skipped instead of shown.
- **Stats** (F3): its own window, laid out like your tiles or as a sortable table: frames shown, drops, decoder, buffer, network, and the GPU video-decoder load. If the decoder runs full and frames drop, streams move to a second NVIDIA GPU or the CPU.
- **Everything comes back:** tabs, layouts, chats, quality, volume and the window place are saved and restored.
- **Setup or portable:** a per-user setup (no admin rights) with a Start menu entry and in-app updates, or a zip to unpack anywhere.
