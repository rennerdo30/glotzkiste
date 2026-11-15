# Glotzkiste design

A multi-stream live viewer: tabs, each tab a free tile layout of stream and chat panes, third-party emotes, ad-free playback through the viewer's own account. Windows x64, one executable. Speed is P0 (`AGENTS.md`).

## 1. Threads and data flow

```
 UI thread (winit + egui)          render thread (gk-render)            mpv threads (per stream)
 input, layout, egui pass  ──UiFrame──▶ D3D11 device + swap chain  ◀──update cb── decode (d3d11va)
 only when the UI changed          renders each stream into its             on the SAME device
        ▲                          tile-sized texture, replays the
        │ UserEvent (proxy)        last tessellated UI, presents
 worker pool (HTTP, decode)        ≤ 1 present per refresh
 chat thread (IRC over TLS)
```

- **UI thread:** runs the egui pass only on input, a worker/chat result, a layout change or an egui `repaint_after` deadline. Output: an `UiFrame` (tessellated primitives, texture delta, video tile rects in physical pixels). It never touches mpv rendering or network I/O.
- **Render thread:** owns the D3D11 device (created with `VIDEO_SUPPORT`, without `SINGLETHREADED`), a flip-model swap chain (`FLIP_DISCARD`, 2–3 buffers, `FRAME_LATENCY_WAITABLE_OBJECT`, max latency 1, tearing off), the egui D3D11 renderer and one `VideoSurface` per stream (mpv render context + texture + SRV). It wakes on: an mpv update callback, a new `UiFrame`, a resize, an animation deadline, quit. On wake: render each stream whose `update()` reports a frame into its texture, then draw the **last** UI primitives (video tiles are egui image meshes with `TextureId::User(stream_id)`, so they pick up the new texture), present, `report_swap`. Several streams finishing frames in the same refresh produce one present.
- **Animated emotes** never trigger an egui pass: each animated emote is one `TextureId::User(ANIM_BIT | n)` whose frames live on the render thread; it picks the frame for the present time and schedules its own wake at the next frame change, but only for animations drawn in the last `UiFrame`.
- **Workers:** a small fixed pool (4 threads) for HTTP and image decode, one chat thread. Results go back as `UserEvent`s through the winit `EventLoopProxy`; every event coalesces into at most one egui pass per refresh.

## 2. Crates and contracts

Public APIs below are the contracts between crates.

### `gk-mpv` (from Nitrate, done)
libmpv loaded at runtime, `Mpv` client handle, `RenderContext::new_d3d11`, `render_d3d11(texture)`, update callback. `Libmpv::load_from(&[path])` loads the extracted embedded DLL.

### `gk-twitch` (blocking, no UI deps; called from workers)
- `auth`: `parse_cookies_txt(&str) -> Option<String>` (Netscape `cookies.txt`, the `auth-token` cookie of `twitch.tv`/`.twitch.tv`; also accept a raw token pasted alone or a JSON cookie export), `Auth { token, login, user_id }` with a `Debug` that hides the token, `validate(&Http, token) -> Result<Auth>`, `store::{save, load, clear}` (DPAPI, `%APPDATA%\Glotzkiste\auth.bin`).
- `Client` (shared `ureq::Agent`, optional `Auth`, a stable random device id): 
  - `channel(login) -> ChannelInfo { id, login, display_name, live, title, category, viewers, avatar_url }`
  - `channels(&[login])` batched; `followed_live() -> Vec<ChannelInfo>` (needs auth)
  - `playback(login) -> Playback { variants: Vec<Variant { name, width, height, fps, bandwidth, url }>, turbo: bool, ads_expected: bool }` — playback access token (GQL, `OAuth` header when logged in, which is what removes ads for Turbo accounts), usher master playlist, parsed variants (source first). Variant URLs are what mpv plays.
  - `global_emotes() / channel_emotes(channel_id) -> Vec<Emote>` for 7TV, BTTV and FFZ: `Emote { name, provider, id, url /* 2x webp or png */, animated, zero_width, width, height }`. Priority on name clash: channel 7TV > channel BTTV > channel FFZ > global 7TV > global BTTV > global FFZ.
  - `badges(channel_id)` (global + channel badge image URLs by `set/version`).
- `image::fetch_decoded(&Http, url) -> DecodedImage { frames: Vec<Frame { rgba, width, height, delay_ms }> }` with caps (≤ 4 MB download, ≤ 256×256 after downscale, ≤ 300 frames), disk cache `%LOCALAPPDATA%\Glotzkiste\cache` bounded at 256 MB.
- Errors are values (`Error` enum), never panics. The token is never logged.

### `gk-chat` (one background thread, no UI deps)
- `ChatClient::start(login: Option<Credentials>, on_event: impl Fn(ChatEvent) + Send + 'static) -> ChatClient`; `join(channel)`, `part(channel)`, `send(channel, text)`, `set_credentials(...)` (reconnects), `Drop` stops the thread. One TLS connection to `irc.chat.twitch.tv:6697` for all channels, anonymous `justinfan<n>` when logged out. Caps `twitch.tv/tags twitch.tv/commands`; PING/PONG, `RECONNECT`, exponential reconnect with rejoin; join rate limit respected.
- `ChatEvent`: `Connected`, `Joined { channel }`, `Message(ChatMessage)`, `UserNotice { message, system_text }`, `ClearChat { channel, user: Option<String> }`, `ClearMsg { channel, id }`, `RoomState { channel, room_id }`, `Notice { channel, text }`, `Disconnected { reason }`.
- `ChatMessage { id, channel, room_id, login, display_name, color: Option<[u8;3]>, badges: Vec<(String, String)>, text, twitch_emotes: Vec<(String, Range<usize>)> /* byte ranges */, action, highlighted, timestamp_ms, reply_to: Option<(String, String)> }`.
- `tokenize(&ChatMessage, &dyn EmoteLookup, own_login) -> Vec<Span>`: `Text`, `Emote { url, name, animated, zero_width }` (Twitch emotes from tags, URL `https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0`), `Mention`, `Link`. Zero-width emotes stack on the previous emote.
- `ChatBuffer`: bounded ring per channel (default 1,000 messages), applies `ClearChat`/`ClearMsg`.

### `gk-render` (Windows only)
- `Presenter::start(hwnd, size_px) -> Result<(Presenter, DeviceInfo)>`, `add_stream(id: u64, mpv: &Mpv)` (the render thread creates the render context on its device), `remove_stream(id)`, `submit(UiFrame)`, `set_animation(id, Arc<AnimFrames>)`, `remove_animation(id)`, `resize(w, h)`, `stats() -> Arc<Stats>` (presents, ui_frames, video frames per stream, skipped presents).
- `UiFrame { primitives: Vec<egui::ClippedPrimitive>, textures_delta: egui::TexturesDelta, pixels_per_point: f32, videos: Vec<VideoTile { id, rect_px: [i32; 4] }> }`. A stream not listed in `videos` is not rendered.
- Its own egui D3D11 renderer (premultiplied alpha, sRGB-correct, scissor per primitive, dynamic vertex/index buffers grown geometrically, managed and user textures).
- Stream textures are sized to the tile's physical pixels and recreated on change; mpv draws aspect-fit with black bars.

### `glotzkiste` (the binary)
- Extracts the embedded libmpv once, creates the window, the presenter and the egui context, owns the model:
  - `Tab { title, tree: egui_tiles::Tree<Pane> }`, `Pane::Stream { channel, quality: Auto | Fixed(name), volume, muted }`, `Pane::Chat { channel }`, `Pane::Empty`.
  - Layout presets (one stream; stream + chat; 2 side by side; 2×2; 2×2 + chat; 1 large + 3 small); tiles are dragged, split and resized freely (egui_tiles), layouts are saved (`%APPDATA%\Glotzkiste\layout.json`).
- Stream manager: per stream an mpv handle (`vo=libmpv`, `hwdec=d3d11va`, muted unless it holds the audio focus), state (resolving, playing, offline, error), automatic variant choice by tile height, refresh of the playback token on error.
- Audio focus: one stream plays sound (click a tile to focus it); per-tile volume.
- Hidden tabs: their streams get `vid=no` (audio stays connected, muted) and get video back on show.
- Login: import a `cookies.txt` or paste a token; shows the account and whether playback is ad-free.
- `--smoke SECS --mute --channel X …` runs offscreen and prints stats.

## 3. Milestones

1. **M1 Foundations (in order, no parallel jobs):** `gk-twitch` playback, then `gk-render` + app skeleton playing N channels from the command line in a 2×2 tile layout, then `gk-chat`.
2. **M2 The viewer:** tabs, tile editor and presets, chat pane with emotes and badges, login, quality and audio focus, persistence.
3. **M3 Polish and proof:** smoke numbers for 1, 4 and 4 + 2 chats; review; release build for the friend.
