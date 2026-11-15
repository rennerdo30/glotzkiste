# Releasing Glotzkiste

Every release needs the owner's yes. CI builds and tests the packages and prepares a **draft** GitHub release. Publishing that draft is the owner's yes; nothing in CI publishes on its own.

## Versions

One version for the whole workspace: `version` under `[workspace.package]` in `Cargo.toml`, following SemVer. Prereleases look like `1.1.0-beta.1`; their tags contain a `-` and become GitHub prereleases.

## Cutting a release

1. **Prepare the commit on `main`:** set the workspace version, move the `## Unreleased` notes in [`CHANGELOG.md`](../CHANGELOG.md) into a `## X.Y.Z` section, commit, push, and let CI run.
2. **Tag that commit and push the tag:**
   ```sh
   git tag -a v1.0.0 -m "Glotzkiste 1.0.0"
   git push origin refs/tags/v1.0.0
   ```
3. **The release workflow runs** ([`.github/workflows/release.yml`](../.github/workflows/release.yml)):
   - checks that the tag matches the workspace version;
   - builds `glotzkiste.exe` (static C runtime) and puts the pinned `libmpv-2.dll` (`scripts/fetch-libmpv.ps1`, checksum-verified) next to it in both packages;
   - packages `glotzkiste-<version>-setup.exe` (per-user NSIS 3.13 setup) and `glotzkiste-<version>-windows-x64.zip`, each with `LICENSE.txt`, `THIRD-PARTY-NOTICES.txt` (cargo-about), `libmpv-NOTICE.txt` and `LGPL-2.1.txt`;
   - runs the installer lifecycle test on the disposable runner (install, reinstall, locked exe, uninstall keeps the user's layout);
   - writes `SHA256SUMS.txt` over both packages and creates a **draft** release with the changelog section as notes. Rerunning it with the same tag refreshes a draft and never touches a published release.
4. **The owner reviews the draft and publishes it.**

A new libmpv build means: update the tag, file name and SHA-256 in `scripts/fetch-libmpv.ps1` and the commit and links in `licenses/libmpv-NOTICE.txt` together.

## Updates in the app

- An installed release build (in `%LOCALAPPDATA%\Programs\Glotzkiste`) checks GitHub releases once after start, at most once a day, on a worker thread. Development builds, `--smoke` runs and `GLOTZKISTE_NO_UPDATE` never check. About (⋯) has *Check now*.
- A found update shows a green button in the top bar. A click (or **Ctrl+Shift+U**) downloads the setup and `SHA256SUMS.txt` into a fresh temporary folder, checks the SHA-256, starts the setup with `/S /UPDATE` and closes; the setup waits for Glotzkiste, installs and starts it again. Nothing installs without that click. Portable copies only point to the download page.
- **Channels:** a prerelease build follows beta (newest published release, prerelease or stable), a stable build follows stable; `GLOTZKISTE_UPDATE_CHANNEL=stable|beta` overrides.
- Only GitHub's API for `rennerdo30/glotzkiste` and its release downloads are fetched, HTTPS only, size-capped, 60 s timeouts.
- The checksum protects integrity, not authenticity (it comes from the same release). The setup is not code-signed, so SmartScreen warns on first run; signing is a separate owner decision.

The setup's file name `glotzkiste-<version>-setup.exe` and `SHA256SUMS.txt` are the contract with the updater; keep both names.

## Building the packages locally

`scripts\package.ps1` builds the release binary (fetching the pinned libmpv), the setup, the zip and `SHA256SUMS.txt` into `artifacts\installer`. It uses the portable NSIS 3.13 at `E:/Development/_cache/tools/nsis-3.13/`; `scripts/fetch-nsis.ps1` downloads the pinned ZIP elsewhere. Without cargo-about it writes a placeholder notice; release builds always generate the real one in CI.

**Never run `scripts/test-installer.ps1` or a built setup on a real PC.** The test installs and uninstalls for the current user, and refuses to run outside GitHub Actions.
