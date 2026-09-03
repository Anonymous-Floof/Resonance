# The `networked` branch

**You are not on `main`. Read this before changing anything.**

Resonance has two branches with deliberately opposed motives. This is not a
feature branch waiting to be merged back — it is a long-lived sibling.

| | `main` | `networked` (here) |
|---|---|---|
| Network access | **None, ever.** No HTTP client in the binary. | Allowed, disclosed, and optional. |
| Promise to the user | "Nothing leaves your machine." | "Here is exactly what leaves, and how to turn it off." |
| Where features come from | Tags on disk, offline analysis. | The above, plus the network. |

## Why `main` is absolute about it

Not squeamishness — it is what makes the claim checkable. `main` says *"there
is no HTTP client in the binary, so there is nothing to opt out of."* That
sentence is worth more than a settings page full of switches, and it survives
only while it is literally true. One `reqwest` in the tree and every privacy
claim in the README, the release notes and the Settings page becomes a lie at
once.

So `main` does not get a "networking off by default" mode. That is what this
branch is for.

## What this branch is for

Everything the offline constraint made impossible:

- Lyrics fetching
- Artwork fetching for tracks with none embedded and no sidecar
- Artist and genre metadata, MusicBrainz-style
- Whatever else you have planned

The motive here is *openness*, not permissiveness. A networked build that is
vague about what it sends is worse than an offline one; it should be able to
tell the user precisely what it asked for, from whom, and when.

## Merge policy

**`main` → `networked`: routine, and your responsibility.**
Decode, DSP, the queue, the library schema, the UI shell, bug fixes — all of
that is developed on `main` and flows here. Merge often:

```bash
git merge main
```

Merging often is much cheaper than merging late. The longer you go, the more
likely a conflict lands in a file you have restructured.

**`networked` → `main`: almost never.**
Never merge this branch wholesale — it would carry the HTTP client across and
break `main`'s reason to exist. If something genuinely offline-neutral gets
written here (an engine fix you happened to hit first), cherry-pick that one
commit:

```bash
git -C "../Music Player" cherry-pick <sha>
```

**Keep the diff small on purpose.** The more of this branch that lives in *new*
files, the less merging ever hurts. Prefer a new `crates/mp-net/` over
scattering request code through `mp-core`. Where existing code must change,
prefer adding a trait or an injection point on `main` first — a seam both
branches share — over editing the same lines here.

## Three things `main` learned the hard way

These were real bugs, each found by a user noticing something was off. Do not
re-introduce them here.

**1. Never ship a setting before the thing it controls works.**
`main` once had exactly the settings this branch will want — `online_metadata`,
`use_musicbrainz`, `use_lastfm`, `fetch_missing_artwork`, `cache_ttl_days`, all
under `[privacy]`. Every one rendered a real control. Not one was ever read by
any code. They were deleted in `03f240e` ("Build the settings that did nothing,
delete the rest") after the user asked how the Last.fm feature worked and the
answer turned out to be "it doesn't."

You will want those names back. Add each one **in the same commit that makes it
do something**, never before.

**2. The docs are part of the product, and they drift silently.**
`main` claimed crossfade, a sleep timer and "resume position on long tracks" in
the README while two of the three did not exist. Anything you write in the
README on this branch about what is fetched must be true of the binary at that
commit.

**3. Resonance never modifies the user's audio files.**
That rule is not suspended here. Fetched artwork and lyrics go to the app's own
cache, not into the music folders, and not into tags — unless tag editing is
explicitly enabled, which is off by default and journalled for undo.

## Claims that must change before this branch ships anything

These are true right now (there is still no HTTP client). They stop being true
the moment the first request is made, and they must be updated **in that same
commit**:

- `README.md` — "No network access at all", "with no network access and no
  account", "nothing is looked up online", and the Privacy bullet under
  *Listening statistics*
- The release notes (`Safety` section)
- `crates/mp-core/src/config.rs` — the doc comment on `struct Privacy` argues at
  length that network settings would be controls over something that does not
  exist. On this branch it will
- The app name or version string should distinguish the two builds, so a user
  can tell which one they are running without reading the settings page

## Suggested shape, not a mandate

- **One crate, `crates/mp-net/`**, holding every outbound request. If networking
  is reachable from anywhere else, nobody can audit it — including you.
- **Off on first run.** Opt-in, with a screen that says what each source is and
  what it will send. `main`'s first-run flow already exists to build on.
- **Cache aggressively to disk.** Refetching on every launch is both rude to
  free APIs and slow. `cache_ttl_days` was the right instinct.
- **A real User-Agent with contact details.** MusicBrainz requires one and will
  block you without it; every other service appreciates it.
- **Rate-limit and back off.** MusicBrainz is one request per second.
- **An activity log the user can actually read** — what was requested, from
  where, and when. This is the feature that makes "open about it" true rather
  than claimed.
- **Never block playback on a request.** Everything network-shaped is a
  background enrichment that can fail silently and be retried later.

## Working across the two

Both worktrees share one repository and one set of branches:

```
Music Player/           main        offline
Resonance-networked/    networked   this branch
```

They have **separate `target/` directories**, so a full build here costs its own
tens of gigabytes. That is the right trade — sharing one would make every switch
between branches a full rebuild — but it is worth knowing before your disk
fills.

Commits, tags and remotes are shared. `git fetch` in either updates both.
