# Resonance — `networked` branch

Read **[BRANCH.md](BRANCH.md)** first. It is short and it is the point of this
branch.

## The one-line version

`main` is offline-absolute — no HTTP client in the binary, and that claim is
printed in the README, the release notes and the doc comment on `struct
Privacy`. **This branch deliberately breaks that constraint, openly.** It is a
permanent sibling of `main`, not a feature branch awaiting merge.

If a feature needs the network — lyrics, artwork, artist metadata — it belongs
here, and `main` should be told to send it here rather than building a
disabled-by-default version over there.

## Rules that hold on both branches

- **Never modify the user's audio files.** Read-only by default. Tag editing is
  opt-in, off by default, confirmed, and journalled for undo. Fetched artwork
  and lyrics go to the app's own cache, never into the music folders.
- **Never ship a setting before the feature behind it works.** `main` once had
  five network settings that rendered real controls and were read by nothing;
  they were deleted in `03f240e` after the user asked how the Last.fm feature
  worked and the answer was that it didn't. This branch will want those exact
  names back — add each in the commit that gives it an effect.
- **Never let a doc claim outrun the binary.** If it is written down, it is true
  at that commit.
- **Never block playback.** Nothing in the UI or library layer may stall the
  audio thread, and nothing network-shaped may make playback wait on it.

## Merging

- `git merge main` — routine and expected. Engine and UI work happens on `main`
  and flows here. Merge often; late merges hurt more.
- Merging this branch *into* `main` is wrong — it would carry the HTTP client
  across. Cherry-pick individual offline-neutral commits instead.
- Prefer new files (`crates/mp-net/`) over edits to shared ones. It is what
  keeps the merges cheap.

## Layout

```
src/main.rs          thin launcher: paths, logging, config, hand off to the UI
build.rs             compiles the icon and version block into the executable
assets/              the application icon, and the script that generates it
crates/
├── mp-core/         settings, paths, colour, and the library
├── mp-audio/        decode → resample → DSP → output; queue, device, analysis
├── mp-net/          outbound requests and the log of them — no transport yet,
│                    and nothing depends on it
└── mp-ui/           theme, shell, views, widgets, visualizers
```

`mp-core` has no UI or audio dependencies and unit-tests without a window or a
sound device; `mp-audio` is free of UI types and can be driven headlessly. Keep
both true — it is what makes the tests runnable at all.

`mp-net` exists now, and **every outbound request goes in it**. No other crate
may take an HTTP dependency; the moment one does, `cargo tree` stops being a
complete answer to what the application can talk to, and the activity log
becomes a partial record that looks like a whole one. It has no transport yet
and nothing depends on it — read its `lib.rs` before adding one.

## Checks before any commit

```bash
cargo test --workspace          # 760 tests
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
- This is a worktree. `../Music Player` is the `main` checkout, sharing one
  repository. Separate `target/` directories, shared commits and remotes.
