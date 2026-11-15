# Glotzkiste repository rules

Read `E:/Development/AGENTS.md` and `E:/Development/general-agent-rules.md` first. They win over anything here unless this file is stricter.

- **Product:** Glotzkiste (codename, "goggle box"), a private multi-stream viewer for one of the owner's friends: several live streams and chats at once, in tabs and free tile layouts, with third-party emotes (7TV first, BTTV, FFZ) and ad-free playback through the viewer's own logged-in account. Windows x64 only. One self-contained executable. Base: the owner's media player Nitrate (`E:/Development/nitrate`): `gk-mpv` is Nitrate's `nitrate-mpv`, and the D3D11 path follows Nitrate's ADR 0001. Architecture and crate contracts: `docs/DESIGN.md`.
- **P0 is speed** (owner, 2026-11-15: "it needs to be max performance ... it shouldn't be allowed to lag"). The performance rules below hold for every change; breaking one is a defect even with green tests.
- **Private:** no remote and no publication without the owner's yes. Commit only as `rennerdo30 <9086097+rennerdo30@users.noreply.github.com>`. No AI attribution lines.
- **Secrets:** the viewer's login token is a credential. Code stores it only DPAPI-protected (`CryptProtectData`, current user) and never logs or prints it. Agents never read, print or commit a real token or cookie file; tests use fake tokens.
- **Pair Desk:** project `glotzkiste` (prefix `GK`) once it exists; until then the plan lives in `docs/DESIGN.md` milestones.
- Paths: always `E:`; `D:` is the same drive. Branch limit: an active branch has a commit in the last 24 h; up to 10 is normal, 11 to 20 needs a recorded reason, 20 is the maximum.

## Performance rules (owner, 2026-11-15, adapted from Nitrate's)

1. **Video never waits for the UI, the UI never waits for I/O.** mpv decodes and renders on its own threads and our render thread. HTTP, chat sockets, emote download and image decoding run on worker threads and post results to the event loop. The UI thread only handles input, layout and painting. No blocking call on the UI or render thread, ever.
2. **Zero-copy video.** Hardware-decoded frames (d3d11va on our shared D3D11 device) go to the screen without a CPU readback. A CPU copy of video frames is a P0 defect.
3. **The UI pass runs only when the UI changed.** Input, a new chat message, a finished download, a layout change. A new video frame or an animated-emote frame never runs the egui pass: the render thread replays the last tessellated UI with the new texture contents. No frame loop of our own; idle with nothing playing means 0 presents and 0 % CPU.
4. **At most one present per display refresh**, however many streams deliver frames. Waitable flip-model swap chain, maximum frame latency 1.
5. **Pixels are not wasted.** Each stream renders at its tile's physical size, and automatic quality picks the smallest variant that fills the tile (a 2×2 grid at 1080p does not decode four source streams). Streams in hidden tabs stop decoding video.
6. **Bounded memory.** Chat history, emote caches (CPU and GPU) and image downloads have caps; nothing grows with watch time.
7. **Never crash on network data or the system.** No `unwrap`/`expect` on I/O, JSON, IRC, images, mpv results or settings. A panicking worker is caught and reported; the window stays alive.
8. **Measured, not assumed.** Present count, dropped frames, UI pass count and CPU time are recorded by `--smoke`; never claim a speed you did not measure (machine, commit, streams).
9. **Small dependency graph, no async runtime.** Worker threads with channels. Every new dependency earns its place.

## Build

- `.cargo/config.toml` is machine-local (template `.cargo/config.example.toml`); it carries `+crt-static`, `target-cpu=x86-64-v3`, rust-lld, the shared sccache and the target dir `E:/Development/_cache/cargo-target/glotzkiste`.
- libmpv is embedded zstd-compressed (`GK_LIBMPV`, pinned build from `scripts/fetch-libmpv.ps1`) and extracted once to `%LOCALAPPDATA%\Glotzkiste\libmpv-<hash>.dll`.
- **Agent-launched runs:** always `--mute`, offscreen/hidden without activation (`--smoke`), with a timeout, own PID recorded and closed. Never a visible window or sound on the owner's PC. Live Twitch access from tests: anonymous only, a handful of requests, never in loops.
- Focused tests only on the owner PC; no full-suite or load runs.
