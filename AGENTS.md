# Glotzkiste repository rules

Read `E:/Development/AGENTS.md` and `E:/Development/general-agent-rules.md` first. They win over anything here unless this file is stricter.

- **Product:** Glotzkiste ("goggle box"), a multi-stream live viewer: several live streams and chats at once, in tabs and free tile layouts, with third-party emotes (7TV first, BTTV, FFZ) and ad-free playback through the viewer's own logged-in account. Windows x64 only. A per-user setup (and a portable zip): `glotzkiste.exe` with `libmpv-2.dll` next to it (owner, 2026-11-16: "since we are now using a setup, we don't need to link statically anymore"). Base: the owner's media player Nitrate (`E:/Development/nitrate`): `gk-mpv` is Nitrate's `nitrate-mpv`, and the D3D11 path follows Nitrate's ADR 0001. Architecture and crate contracts: `docs/DESIGN.md`.
- **P0 is speed** (owner, 2026-11-15: "it needs to be max performance ... it shouldn't be allowed to lag"). The performance rules below hold for every change; breaking one is a defect even with green tests.
- **Repository:** public at `https://github.com/rennerdo30/glotzkiste` (owner, 2026-11-15: "release it publicly on GitHub"). Every release needs the owner's yes; releases follow `docs/RELEASING.md`. Commit only as `rennerdo30 <9086097+rennerdo30@users.noreply.github.com>`. No AI attribution lines. Push with explicit refs only: `git push origin HEAD:refs/heads/<branch>`.
- **Public from day one:** no secrets, no owner-PC config in committed files (`.cargo/config.toml` is local), no third-party code, media or assets. Other services are named only factually where the app needs it (the streaming site it plays, the emote providers), never with their logos or brand art, and the README says the project is not affiliated with them. Screenshots come from `--demo` (generated video, sample chat), never from real streams.
- **Licences:** MIT. Rust dependencies permissive only (`deny.toml`). libmpv (LGPL) ships as `libmpv-2.dll` next to the exe and is replaceable; packages carry `licenses/` (LGPL text, libmpv notice and source offer).
- **Secrets:** the viewer's login token is a credential. Code stores it only DPAPI-protected (`CryptProtectData`, current user) and never logs or prints it. Agents never read, print or commit a real token or cookie file; tests use fake tokens.
- **Pair Desk:** project `glotzkiste` (prefix `GK`, created 2026-11-16): open issues, owner checks and the handoff live there; `.pair-desk.json` (local, gitignored) links the repo.
- **No subagents, no parallel jobs** (owner, 2026-11-15: "NO PARALLEL JOBS NO SUBAGENTS!"): the main session builds this project itself, one step at a time, on `main`.
- Paths: always `E:`; `D:` is the same drive. Branch limit: an active branch has a commit in the last 24 h; up to 10 is normal, 11 to 20 needs a recorded reason, 20 is the maximum.

## Performance rules (owner, 2026-11-15, adapted from Nitrate's)

1. **Video never waits for the UI, the UI never waits for I/O.** mpv decodes and renders on its own threads and our render thread. HTTP, chat sockets, emote download and image decoding run on worker threads and post results to the event loop. The UI thread only handles input, layout and painting. No blocking call on the UI or render thread, ever.
2. **Zero-copy video on the main GPU.** Hardware-decoded frames (d3d11va on our shared D3D11 device) go to the screen without a CPU readback; a CPU copy of a main-GPU stream is a P0 defect. The one exception is streams on another GPU (owner, 2026-11-15/16: "If the second GPU needs CPU copies, then we have no other choice"; other GPUs are loaded first, "the main GPU would be loaded at last"): they decode and scale there, and each frame is copied over once at tile size: GPU to GPU through a cross-adapter heap (owner, 2026-11-17: "i want you to build it"), else read back without waiting for that GPU.
3. **The UI pass runs only when the UI changed.** Input, a new chat message, a finished download, a layout change. A new video frame or an animated-emote frame never runs the egui pass: the render thread replays the last tessellated UI with the new texture contents. No frame loop of our own; idle with nothing playing means 0 presents and 0 % CPU.
4. **At most one present per display refresh**, however many streams deliver frames. Waitable flip-model swap chain, maximum frame latency 1.
5. **Pixels are not wasted.** Each stream renders at its tile's physical size, and automatic quality picks the smallest variant that fills the tile (a 2×2 grid at 1080p does not decode four source streams). Streams in hidden tabs stop decoding video.
6. **Bounded memory.** Chat history, emote caches (CPU and GPU) and image downloads have caps; nothing grows with watch time.
7. **Never crash on network data or the system.** No `unwrap`/`expect` on I/O, JSON, IRC, images, mpv results or settings. A panicking worker is caught and reported; the window stays alive.
8. **Measured, not assumed.** Present count, dropped frames, UI pass count and CPU time are recorded by `--smoke`; never claim a speed you did not measure (machine, commit, streams).
9. **Small dependency graph, no async runtime.** Worker threads with channels. Every new dependency earns its place.

## Build

- `.cargo/config.toml` is machine-local (template `.cargo/config.example.toml`); it carries `+crt-static`, `target-cpu=x86-64-v3`, rust-lld, the shared sccache and the target dir `E:/Development/_cache/cargo-target/glotzkiste`.
- libmpv is the pinned build from `scripts/fetch-libmpv.ps1`, shipped as `libmpv-2.dll` next to the exe; `GK_LIBMPV` (local config) makes build.rs copy it next to development builds. The C runtime stays linked statically (`+crt-static`): no Visual C++ runtime to install.
- **Agent-launched runs:** always `--mute`, offscreen/hidden without activation (`--smoke`), with a timeout, own PID recorded and closed. Never a visible window or sound on the owner's PC. Live Twitch access from tests: anonymous only, a handful of requests, never in loops.
- Focused tests only on the owner PC; no full-suite or load runs.
