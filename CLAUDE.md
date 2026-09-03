# Resonance — `networked` branch

Read **[BRANCH.md](BRANCH.md)** first. It is short and it is the point of this
branch.

## The one-line version

`main` is offline-absolute — no HTTP client in the binary, and that claim is
printed in the README, the release notes and the Settings page. **This branch
deliberately breaks that constraint, openly.** It is a permanent sibling of
`main`, not a feature branch awaiting merge.

## Rules that survive the branch

- **Never modify the user's audio files.** Fetched artwork and lyrics go to the
  app's own cache, never into the music folders and never into tags unless tag
  editing is explicitly enabled (off by default, journalled for undo).
- **Never ship a setting before the feature behind it works.** `main` shipped
  five network settings that nothing read; they were deleted in `03f240e`. Add
  each back in the same commit that gives it an effect.
- **Never let a doc claim outrun the binary.** If the README says something is
  fetched, it is fetched at that commit.
- **Never block playback on a request.** Networking is background enrichment
  that may fail silently.

## Merging

- `git merge main` — routine and expected. Engine and UI work happens on `main`.
- Merging this branch *into* `main` is wrong. Cherry-pick individual
  offline-neutral commits instead.
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
└── mp-ui/           theme, shell, views, widgets, visualizers
```

`mp-core` has no UI or audio dependencies and unit-tests without a window or a
sound device. Keep it that way; add a `mp-net` rather than widening `mp-core`.

## Checks before any commit

```bash
cargo test --workspace          # 707 tests at the branch point
cargo clippy --workspace --all-targets
cargo fmt --all
```

All three are clean at the branch point. Keep them that way.

## Environment notes

- Windows-first. Rust 1.88+ (edition 2024, let-chains).
- Tests must leave no temp directories behind — use `tempfile::TempDir`, never a
  hand-built path in `std::env::temp_dir()`. This was fixed in `3f14c08` after
  206 directories accumulated.
- This is a worktree. `../Music Player` is the `main` checkout, sharing one
  repository. Separate `target/` directories, shared commits and remotes.
