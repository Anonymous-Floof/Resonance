# Resonance — `main` branch

## The one rule this branch exists for

**No network access. Ever. There is no HTTP client in the binary and there must
not be one.**

This is not a default, a preference, or a setting that ships switched off. It is
the thing that makes the privacy claim checkable instead of merely stated:

> There is no HTTP client in the binary, so there is nothing to opt out of.

That sentence appears in the README, in the release notes, and argued at length
in the doc comment on `struct Privacy` in `crates/mp-core/src/config.rs`. One
`reqwest`, `ureq`, `curl` or hand-rolled `TcpStream` in the tree makes all of
them false at once. Adding a dependency that *could* reach the network breaks
this even if nothing calls it.

If a feature needs the network — lyrics, artwork, artist metadata, anything —
it does not belong here. It belongs on the **`networked`** branch, which exists
for exactly that and is checked out at `../Resonance-networked`. Say so and move
on; do not build a disabled-by-default version here.

## Rules that hold on both branches

- **Never modify the user's audio files.** Read-only by default. Tag editing is
  opt-in, off by default, confirmed, and journalled for undo.
- **Never ship a setting before the feature behind it works.** This branch once
  had five network settings that rendered real controls and were read by
  nothing; they were deleted in `03f240e` after the user asked how the Last.fm
  feature worked and the answer was that it didn't. A control that lies is
  worse than a short settings page.
- **Never let a doc claim outrun the binary.** The README claimed crossfade, a
  sleep timer, and "resume position on long tracks" while two of the three did
  not exist. If it is written down, it is true at that commit.
- **Never block playback.** Nothing in the UI or library layer may stall the
  audio thread.

## Merging with `networked`

- `main` → `networked` is routine; that branch merges from here.
- **Never merge `networked` into `main`** — it would carry the HTTP client
  across and end the reason this branch exists. If an offline-neutral fix gets
  written over there, cherry-pick that single commit.

Both are worktrees of one repository, sharing commits, tags and remotes, with
separate `target/` directories.

## Layout

```
src/main.rs          thin launcher: paths, logging, config, hand off to the UI
build.rs             compiles the icon and version block into the executable
assets/              the application icon, and the script that generates it
crates/
├── mp-core/         settings, paths, colour, and the library
├── mp-audio/        decode → resample → DSP → output; queue, device, analysis
└── mp-ui/           theme, shell, views, widgets, visualizers
```

`mp-core` has no UI or audio dependencies and unit-tests without a window or a
sound device; `mp-audio` is free of UI types and can be driven headlessly. Keep
both true — it is what makes the tests runnable at all.

See the README's *How it is put together* for the reasoning behind the split.

## Checks before any commit

```bash
cargo test --workspace          # 707 tests
cargo clippy --workspace --all-targets
cargo fmt --all
```

All three are clean. Keep them that way — clippy included, and prefer fixing the
cause over an `#[allow]`.

## Gotchas that have each cost real time

- **egui only repaints on demand.** An idle window produces no frames, so
  anything frame-driven — a timer, a debounce, a periodic flush — simply stops.
  Request a repaint explicitly, or drive it from wall-clock time.
- **Backslash line-continuations in string literals get collapsed into runs of
  literal spaces** by the editing pipeline. This has shipped to the user twice.
  Write single-line string literals; if you see a suspicious gap in UI text,
  check with `cat -A` before theorising.
- **Crossfade schedules against the *decoded* position, not the playback
  position.** Decoding runs up to a ring buffer ahead. `seek_to` must update
  `decoded_frames` or the fade is scheduled past the end of the track.
- **Tests must leave no temp directories behind.** Use `tempfile::TempDir`,
  never a hand-built path under `std::env::temp_dir()`. 206 directories
  accumulated before this was fixed in `3f14c08`.
- **Audio faults are invisible to unit tests.** There are headless diagnostic
  examples for exactly this — `crossfade_check`, `silence_probe`,
  `playback_check` and others; see the README's *Diagnostic examples*. Reach for
  one before guessing.

## Environment

- Windows-first, portable core. Rust 1.88+ (edition 2024, let-chains).
- The user runs on this machine. **Do not launch the app or take over the
  screen without asking** — offer manual check instructions instead, or ask them
  to hand over control.
- `watched_folders` points at a real music collection. Nothing in it has ever
  been modified, and nothing should be.
