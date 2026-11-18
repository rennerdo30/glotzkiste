# Changelog

## Unreleased

## 1.2.1

- **On crates.io:** `cargo install glotzkiste` builds the app; its parts are published as `glotzkiste-mpv`, `glotzkiste-twitch`, `glotzkiste-chat`, `glotzkiste-update` and `glotzkiste-render`. Nothing changes in the app itself; it needs `libmpv-2.dll` next to it or on `PATH` (from the portable zip).

## 1.2.0

- **A cleaner look:** the logo in the top bar, pill tabs with an accent underline and stream count badges, one style for every icon button, a search field for adding channels, rounded tiles (the video too) with more air between them, a soft shade behind a tile's hover bar instead of a hard strip, a card for empty tabs (with "Random live streams"), roomier menus and dialogs with shadows.
- **Full decoder relief:** a video decoder at 92 % or more counts as overloaded even without rising drops (late frames are skipped, not dropped); the valve then moves up to three streams per step: to a GPU with room, else the smallest ones (480p or less, cheap in software) to the CPU. Stats name the saturated decoder (90 % or more) instead of "Display" for streams on the main GPU, and the decoded total counts at least the frames each stream shows.
- Two unfinished frames per stream on the GPU (four shared textures per stream): with a full decoder one frame in flight capped every stream at about 12 fps. Balancing moves up to four streams every two seconds while a video decoder is at 90 % or more.
- **Starting a hundred streams:** a stream waits for its render worker (up to 30 s) instead of falling back to the render thread after 3 s. The fallback put half of all streams on the render thread (91 % of a core, 24 presents/s) and gave some players two render contexts (mpv error -20). Compiled shaders are cached on disk.
- **Full speed in the background:** Glotzkiste opts out of Windows' power throttling (EcoQoS) and keeps its 1 ms timers while the window is hidden or minimized; the render threads run as multimedia playback threads (MMCSS).
- **Overloaded decoders no longer starve every tile:** each stream keeps at most one unfinished frame on the GPU (a frame is skipped while the previous one is still being drawn), so the render workers stay responsive and the window always has finished frames to show (GK-20, second part).
- A stream the decoder valve or the balancing moved is not moved again for a minute (a failing move used to repeat every two seconds).
- **Smooth under load:** the window no longer waits on the GPU for streams whose newest frame is not finished yet (a full video decoder used to hold back every present: a 110-stream test showed 12 presents/s); such a stream shows its previous frame for a moment instead (GK-20).
- **Closing is instant**, also with a hundred streams: the layout and settings are saved, the window disappears and the process ends without tearing every player down one by one (GK-21).
- Stats: the top row shows the frames decoded per second over all streams, and how many reach the tiles.
- Resizing a grid tile shows an outline while dragging and changes the grid once on release; the tiles glide to their new places and keep their render size while they move (GK-16).
- **Resize one grid tile:** in a grid, drag a tile's bottom-right corner to make it span up to 4×4 cells; the other tiles flow around it, and double-clicking the corner makes it one cell again. A layout preset resets the sizes (GK-16).
- **Several chat panels:** Layout → "+ Chat of …" or "+ Combined chat" stacks more chat panels in the column on the right; layout presets keep every chat panel (GK-17).
- **Layout import and export** (Layout menu): save a tab as a `.json` file and open it again as a new tab, also on another PC (GK-18).
- In a combined chat, the channel you write in follows the stream you listen to; a pick by hand holds until the next switch (GK-19).
- **Settings** (⚙ in the top bar): latency, highest automatic quality, decoding on other GPUs, GPU-to-GPU copy, render threads, daily update check and update channel; saved in `settings.json` beside the layout (GK-15).
- **Random streams** (🎲 next to the channel box): fill the current tab with any number of random live streams, optionally from one category and in one language; `--random N` (with `--random-category`, `--random-language`) does the same at start, handy for load tests (GK-12).

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
