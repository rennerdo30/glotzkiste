# Changelog

## Unreleased

## 1.1.0

- **GPU to GPU:** frames of streams on your other GPUs reach the window GPU to GPU through memory both GPUs share (DirectX 12 cross-adapter heap), without a CPU copy; the render thread does about 40 % less work with four streams on an integrated GPU. Where that is not available, frames are read back as before (GK-3).
- Stats: the decoder column says "GPU copy" or "CPU copy" for streams on another GPU; the copy time counts CPU copies only.

## 1.0.1

- **Balancing:** streams move back toward the preferred GPUs while running: from the CPU to any GPU with room, and from a decoder at 60 % or more to an earlier GPU in order with room (the main GPU last), one stream every six seconds. Streams placed by hand (right-click) stay where they are (GK-2).
- Stats: the download rate is averaged over four seconds instead of reading 0 between segments (GK-5); repeated and redrawn frames are counted for every stream again (GK-6).
- The stats window no longer starts render workers it never uses.
- Release workflow: rebuilding a draft also refreshes its notes.
- The tile's top bar shows the grab cursor only on free space; over its buttons the pointing hand, and a drag starting on a button no longer moves the tile (GK-1).

## 1.0.0

The first release.

- **Many streams at once:** tabs of free tile layouts. Grid, side by side, spotlight and one-at-a-time presets, with or without a chat column; drag tiles by their top bar, drag the gaps to resize, double-click to maximize.
- **Ultra-low latency:** its own live feeder starts at the segment being encoded and streams it into the player as it arrives; latency modes Ultra low, Low and Smooth keep the delay steady by speeding up or easing off slightly.
- **Fast:** hardware decoding straight to the screen without copies; the streams render on two to four worker threads into textures the window shares, each frame at its due time, one present per screen refresh; the interface is redrawn only when something changes. Auto quality follows the tile size, keeps 60 fps and steps down only on a GPU whose video decoder is busy.
- **Every GPU you have:** streams decode on your other GPUs first (integrated ones too) and on the GPU drawing the window last; a full decoder hands streams on to the next GPU with room, the CPU last. Right-click a stream to choose where it decodes.
- **Chat:** live chat with 7TV, BTTV and FFZ emotes (animated), badges, mentions and links; switch a chat to any channel, or combine several (or all streams in the tab) into one.
- **No ads with Turbo:** log in with your browser session, guided step by step (open-source cookie exporter or the browser's developer tools); the login is kept in Windows Credential Manager. Without Turbo, ad breaks are skipped instead of shown.
- **Stats** (F3): its own window, laid out like your tiles or as a full-width sortable table: frames shown, drops, decoder and GPU, buffer, network, each GPU's video-decoder load, and what holds each stream back (network, the broadcast itself, the decoder, the GPU copy or the display).
- **Everything comes back:** tabs, layouts, chats, quality, volume and the window place are saved and restored.
- **Setup or portable:** a per-user setup (no admin rights) with a Start menu entry and in-app updates, or a zip to unpack anywhere.
