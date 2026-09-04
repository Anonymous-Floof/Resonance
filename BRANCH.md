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

- **Lyrics fetching** — done, via LRCLIB
- Artwork fetching for tracks with none embedded and no sidecar
- Artist and genre metadata, MusicBrainz-style
- Whatever else you have planned

Artwork is the natural next one and is considerably harder than lyrics was, for
a reason worth knowing before starting: the Cover Art Archive is addressed by
MusicBrainz release id, so it is two services and a *choice* between candidate
releases rather than one exact-match lookup. Lyrics went first precisely
because `/api/get` matches on artist, title, album and duration at once and so
cannot quietly return the wrong thing.

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

## The claims that had to change, and did

This section was a to-do list until lyrics fetching landed. It is kept as a
record of what the first request cost, because the next feature will owe the
same debt and this is the shape of it.

Changed in the commit that made the first request:

- `README.md` — the banner, the opening description, the *Privacy and safety*
  bullets, and a new *Online lookups* section stating what is sent, to whom,
  when, and where the log of it is
- `crates/mp-core/src/config.rs` — the doc comment on `struct Privacy` argued
  at length that a network setting would control something that does not exist.
  It now introduces `online_lyrics` and explains why it is off
- `crates/mp-ui/src/views/settings.rs` — the Privacy note said there was no
  network client, so there was nothing to opt out of. There is now an *Online*
  section above it
- `crates/mp-ui/src/views/welcome.rs` — the first-run promise said no lookups
  leave your computer. It now says one can, that it is off, and where to look
- The version is `0.2.0-networked`, so the window title, the log, an exported
  bundle and the outgoing `User-Agent` all name the build

Two rules came out of doing it, and both are in `CLAUDE.md`:

- The "what is sent" sentence lives on the `Source` and is *printed* by the
  settings screen rather than retyped into it. A description that is typed
  twice is a description that will disagree with itself.
- No test may open a socket. Fetchers take a `Transport`, and checking a real
  service is what `cargo run --example lyrics_probe` is for.

## The shape it took

This was a list of suggestions. All of it got built, and it held up:

- **One crate, `crates/mp-net/`**, holding every outbound request. If networking
  is reachable from anywhere else, nobody can audit it — including you.
- **Off on first run.** Each feature has its own switch, off by default, beside
  the sentence saying what it would send.
- **Cache aggressively to disk**, misses included — a library is full of tracks
  no service has heard of, and without a negative cache those are a fresh
  request on every launch, forever. Hits never expire; misses last a fortnight.
- **A real User-Agent with contact details.** Name, version and project link.
- **Rate-limit and back off.** A floor between requests taken from the source
  itself, doubling on failure to a five-minute ceiling, cleared by one success.
- **An activity log the user can actually read** — a plain tab-separated text
  file, including the lookups that never left the machine. This is the feature
  that makes "open about it" true rather than claimed.
- **Never block playback on a request.** Everything network-shaped runs on its
  own thread and can fail silently.

Two things the list did not anticipate:

- **Local always wins.** A `.lrc` the user put beside a track is the answer they
  chose, and a fetched copy never replaces it.
- **Say when something was fetched.** Words that came over the network are
  labelled on screen. Showing them identically to the ones found on disk would
  be the build quietly passing off a lookup as something it already had, and
  that is exactly the vagueness this branch exists to avoid.

One thing to carry forward: `cache_ttl_days` is still not a setting, because a
fixed fortnight for misses needed no knob. A constant that does the job is
better than a control that does not need to exist.

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
