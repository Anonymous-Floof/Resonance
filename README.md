> [!IMPORTANT]
> **This is the `networked` branch.** [`main`](../../tree/main) is
> offline-absolute — no HTTP client in the binary at all, so there is nothing
> to opt out of. This build can reach the network, and everything it can reach
> is listed below and switched off until you turn it on.
>
> Right now that is one thing: fetching lyrics for tracks that have none. See
> [Online lookups](#online-lookups), and [BRANCH.md](BRANCH.md) for why the two
> builds exist.

> [!NOTE]
> - This project was built with 99% AI assistance (Claude Opus 5+) under human oversight. 🤖
> - Expect there to be bugs and balancing issues

# Resonance

A modern music player for the collection you already have.

Resonance indexes your local music and plays it — no account, no streaming, and
no lookups you did not ask for. It is built for people whose music lives in
folders on a disk, and who want something better to play it with than what came
with the operating system.

**Windows · Rust · MIT**

---

## Contents

- [What it does](#what-it-does)
- [For listeners](#for-listeners) — install it and use it
  - [Install](#install)
  - [First run](#first-run)
  - [Portable mode](#portable-mode)
  - [Keyboard shortcuts](#keyboard-shortcuts)
  - [Where your files live](#where-your-files-live)
  - [Editing tags safely](#editing-tags-safely)
  - [Online lookups](#online-lookups)
  - [Format support](#format-support)
- [For developers](#for-developers) — build it and change it
  - [Build and run](#build-and-run)
  - [Project layout](#project-layout)
  - [Tests](#tests)
  - [Diagnostic examples](#diagnostic-examples)
  - [How it is put together](#how-it-is-put-together)
- [License](#license)

---

## What it does

**Library**
- Indexes a folder tree once and keeps up with it live as files change
- Browse by Songs, Artists, Albums, Genres or Folders
- Instant full-text search across title, artist, album and genre
- Cleans up messy metadata — `- Topic` channel names, download-site
  watermarks, titles that repeat their own artist — without inventing anything
- Cover art extracted from tags or picked up from `folder.jpg` beside the music
- Finds duplicates by title and artist within a duration tolerance

**Playback**
- Gapless, and crossfade up to 12 seconds with a linear or equal-power curve
- Ten-band equalizer with presets, a limiter, and a live response curve you can
  drag directly
- ReplayGain, per track or per album
- Shuffle — off, true random, or smart shuffle that spaces out artists and
  avoids what you heard recently
- Repeat, and a sleep timer by the clock or to the end of the track
- Optional trimming of digital silence at the start and end of a track
- **Queue panel** showing what is coming next in the order it will actually
  play, including under shuffle — jump to anything, drop anything, or clear it
- Right-click any track for **Play next** or **Add to queue**
- Output device picker and buffer size control, applied without a restart

**Playlists**
- Ordinary playlists, built by adding from your library or from suggestions
- **Smart playlists** — visual rule builder with AND/OR groups, live preview
  count, and a limit and sort ("50 tracks, least recently played")
- **Similar tracks**, worked out entirely offline, with a chip on every
  suggestion saying *why* it was suggested
- **Auto-radio** — when the queue runs dry, keep going from where you are
- Import and export as standard `.m3u8`

**Listening statistics**
- A **Home page** that fills in as you listen: total listening time, plays,
  how much of your library you have actually explored, and a month of activity
- Your most played tracks, artists and albums, and a way straight back into
  anything you were recently listening to
- A play is only counted once you have **actually heard** half the track (or
  four minutes, whichever is sooner), so skipping through an album does not
  inflate anything
- All of it stays on your machine, and it can be switched off entirely in
  Settings under Privacy

**Interface**
- Six visualizers: spectrum bars, oscilloscope, radial, waveform ribbon,
  a GPU aurora shader, and a beat-reactive particle field
- Full-screen now-playing view with large artwork and synced lyrics —
  from the file, from an `.lrc` beside it, or [fetched](#online-lookups)
- **Adaptive theming** — the accent colour is taken from the current cover and
  crossfaded as tracks change
- Optional album-art or visualizer backgrounds behind the content and player bar
- Dark, light, and adaptive themes; three density settings

**Privacy and safety**
- **Never modifies your audio files.** Tag editing is off by default, and every
  edit it does make is reversible from a history panel.
- **Nothing leaves your machine unless you switch it on.** Your library,
  artwork and suggestions are all built locally and always have been. The one
  feature that reaches out is [lyrics fetching](#online-lookups), it is off by
  default, and Settings says exactly what it would send.
- **Every request is written down** in a plain text file you can read, whether
  or not it came back with anything.

---

# For listeners

## Install

Download `resonance.exe` from the [releases page][releases] and run it. There is
no installer and nothing is written outside the directories listed
[below](#where-your-files-live).

[releases]: ../../releases

To build it yourself, see [Build and run](#build-and-run).

## First run

On the first launch Resonance shows a short welcome and asks for a music folder.
Pick the folder your music lives in — subfolders are included — and it will
index everything it can play.

A first scan of a few thousand tracks takes a few seconds. After that, launching
is near-instant: each file carries an `(mtime, size)` fingerprint, so a rescan
only opens files that actually changed.

You can add more folders later in **Settings → Library**.

Resonance opens with a set of defaults that are meant to be usable rather than
neutral: a fairly quiet starting volume, the Rock equalizer curve on, the aurora
visualiser taking its colours from album art, and the library grouped by artist.
Background audio analysis is on as well, which is what makes similar tracks and
auto-radio work — it decodes each track once, in the background, and you can
turn it off in **Settings → Library** if you would rather it did not. All of it
is in **Settings**, and none of it is load-bearing.

## Portable mode

Portable mode makes Resonance leave *nothing* on the host machine, so the whole
app — settings, library index, artwork and logs — can live on a USB stick.

**To turn it on:** create an empty file called `resonance.portable` in the same
folder as `resonance.exe`.

```
E:\Resonance\
├── resonance.exe
└── resonance.portable      ← this file, empty, switches it on
```

Next launch, Resonance creates `Resonance-data\` beside the executable and keeps
everything there instead of in your user profile:

```
E:\Resonance\
├── resonance.exe
├── resonance.portable
└── Resonance-data\
    ├── config\             settings
    ├── data\               library index and logs
    └── cache\              cover thumbnails
```

**To turn it off:** delete `resonance.portable`. Resonance goes back to the
per-user directories and your portable data stays in `Resonance-data\` until you
delete it.

> **Why a file and not a setting?**
> The switch has to be readable *before* Resonance knows where its settings are.
> A setting stored inside the directory it is choosing cannot be read until that
> directory has already been chosen. A file beside the executable is the one
> place that is always findable.

Two things worth knowing:

- The marker must be a **file**, not a folder. If you accidentally create a
  directory with that name it is ignored, deliberately — otherwise a slip would
  silently give you a second, empty library.
- The two libraries are separate. Switching modes does not migrate anything;
  copy `Resonance-data\` yourself if you want to.

## Keyboard shortcuts

| Key | Does |
|---|---|
| `Space` | Play or pause |
| `←` / `→` | Skip back / forward 5 seconds |
| `Ctrl` + `←` / `→` | Previous / next track |
| `↑` / `↓` | Volume |
| `M` | Mute |
| `S` | Shuffle |
| `R` | Repeat |
| `Q` | Queue panel |
| `F11` | Full-screen now playing |
| `Ctrl` + `F` | Search |
| `Esc` | Back out, or clear the search |

Single-key shortcuts are ignored while you are typing in a text box, so you can
search for "smart" without shuffling the queue. `Esc` backs out of one thing at
a time: full screen first, then the search, then a drill-down.

The same list is in **Settings → Keyboard**.

## Where your files live

Resonance writes to these directories and nowhere else. **Nothing is ever
written next to your music.**

| What | Where |
|---|---|
| Settings | `%APPDATA%\Resonance\config\config.toml` |
| Library index | `%APPDATA%\Resonance\data\library.db` |
| Logs | `%APPDATA%\Resonance\data\logs\` |
| Cover thumbnails | `%LOCALAPPDATA%\Resonance\cache\art\` |

(In [portable mode](#portable-mode), all four move to `Resonance-data\` beside
the executable.)

`config.toml` is plain TOML and meant to be hand-editable. Anything out of range
is clamped on load, and a file that will not parse is set aside rather than
overwritten.

**The index is a cache, never a record.** Deleting `library.db` costs you a
rescan and nothing else. Resonance never becomes the only place some piece of
your music information exists — which is also why the recovery path for a
damaged index is to delete it rather than repair it.

## Editing tags safely

Tag editing is **off by default**. Turn it on in **Settings → Library**.

Once on, every edit writes the original tags to an undo journal first, so any
change can be reverted from the history panel. Resonance edits tags *inside*
files and never renames, moves, or reorganises anything on disk.

If a file has been changed by something else since you edited it, the revert
refuses rather than overwriting whatever did it.

## Online lookups

This build can look things up online. It does exactly one thing, it is **off
until you switch it on**, and this section is the whole of what it does.

### Fetching lyrics

**Settings → Online → Fetch lyrics.**

When it is on, opening the full-screen now-playing view on a track that has no
lyrics in its tags and no `.lrc` file beside it asks
[LRCLIB](https://lrclib.net) for them. LRCLIB is free, needs no account, and is
contributed to by the people who use it.

| | |
|---|---|
| **Where it goes** | `lrclib.net`, and nowhere else |
| **What is sent** | The artist, title and album from that track's own tags, and its length in seconds |
| **What is not sent** | Anything about you, your library, your file paths, or what else you listen to. There is no account and no identifier |
| **When** | Only when you open the full-screen view, once per track, never twice |
| **If it fails** | Nothing happens. The track has no lyrics, exactly as before |

Lyrics that arrived this way say so underneath them, so you can always tell
them from the ones that were already on your disk. **Lyrics you provide
yourself always win** — a `.lrc` you put beside a track is never replaced by a
fetched copy, and no request is made for a track that already has words.

Answers are cached, including the misses, so a track is looked up once and then
left alone. **Settings → Online → Clear cached lyrics** forgets them all.

### Try other releases

**Settings → Online → Try other releases.** Off by default.

The lookup above matches artist, title, album *and* length together, so it
either finds the exact recording you are playing or finds nothing at all. That
is what stops it ever putting the wrong words on a song — but it also means one
wrong tag is a miss, and [tags are often wrong](#known-limitation-lookups-are-only-as-good-as-your-tags).

With this on, a track the exact lookup could not find is asked about a second
time using only the artist and title. That is still an exact match on the two
fields that identify the *song*, so it cannot return a different song — only a
different release of the same one.

The trade is timing. Words found this way belong to some other pressing, so if
they are synced they may drift against what you are actually hearing. Plain
lyrics are unaffected. It also costs one extra request, and only on tracks that
have already missed.

### Checking what it actually did

Every lookup is written to a plain text file, one line each, including the ones
that never left your machine:

```
2026-09-04T14:03:11Z	lrclib	lrclib.net	ok	5902	lyrics for "Creep" by Radiohead
2026-09-04T14:05:02Z	lrclib	lrclib.net	not-found	0	lyrics for "untitled 3" by ---
2026-09-04T14:07:44Z	lrclib	lrclib.net	cached	0	lyrics for "Amnesiac" by Radiohead
```

`cached` and `skipped` mean no request was made. **Settings → Online → Show the
activity log** opens it; it lives beside the library index in
[your data folder](#where-your-files-live), it is yours to read or delete, and
deleting it starts a new one rather than switching the logging off.

### Known limitation: lookups are only as good as your tags

**This is the main reason a lookup comes back with nothing, and it is worth
understanding before you conclude the feature is broken.**

Resonance does not send the audio anywhere. It has no idea what a track
*sounds* like, so every online lookup is made from the text in the file's own
tags. When those tags do not match what the rest of the world calls the same
recording, the answer is nothing at all.

Files saved from YouTube are the classic case, and they tend to be wrong in all
four fields at once:

| Field | What a YouTube rip usually has | What the service expects |
|---|---|---|
| Artist | The channel name — `SomeChannel`, `… - Topic`, `…VEVO` | The performing artist |
| Title | Decorated — `(Official Video)`, `[HD]`, `(Lyrics)` | The plain track title |
| Album | Missing, or the channel name again | The release it appeared on |
| Duration | A second or two out, from added intros or trailing silence | The release duration |

Resonance already cleans up what it can *prove* at scan time — `- Topic`
suffixes, download-site watermarks, titles that repeat their own artist — but
it deliberately stops there rather than inventing metadata, so plenty survives.

Two further things make this worse for some collections than others:

- **Covers, remixes and fan songs.** Where many artists have recorded a song
  with the same title, artist and title alone may not be enough to identify it,
  and community catalogues are thinner for this material to begin with. Game
  and fandom music is affected more than most.
- **It applies to everything, not just lyrics.** Artwork and artist metadata
  are planned, and they will hit the same wall — arguably harder, since
  matching a *release* is a fuzzier problem than matching a song.

**What helps**

- Fix the tags. **Settings → Library → Allow tag editing**, then edit the track;
  every change is journalled and reversible. Correct tags fix the lookups
  permanently, and they improve the library, search and grouping too.
- Turn on **Try other releases** (see above), which retries on artist and title
  alone when the exact lookup misses.
- Check the [activity log](#checking-what-it-actually-did). A `not-found` line
  shows the exact artist and title that were sent, which is usually enough to
  see what is wrong with the tags.

Nothing else in Resonance touches the network. Your library, artwork,
suggestions and statistics are all built on your own machine, and always were.

## Format support

Everything `symphonia` decodes: **MP3, AAC/M4A, ALAC, FLAC, Vorbis, WAV, AIFF,
CAF** and Matroska.

**Opus is not supported.** `symphonia` ships no Opus decoder, and adding one
means a C dependency on libopus. Opus files are listed as unplayable *with a
reason* rather than silently disappearing — as is anything else that fails to
decode.

---

# For developers

## Build and run

Requires **Rust 1.88+** (edition 2024 and let-chains).

```bash
cargo run --release
```

For a shipping binary, use the `dist` profile — fat LTO, about 3% smaller, and
several minutes slower to build:

```bash
cargo build --profile dist
```

The Windows icon and version block are compiled in by `build.rs`, which needs
`rc.exe` from the Windows SDK. If it is missing the build still succeeds and
prints a warning — you get a working binary with the generic Windows icon.

The icon itself is generated rather than drawn, so that each size can be tuned
for the pixels it actually has: the 16px version drops two rings and two bars
that would otherwise become a smudge. `assets/icon.ico` and `assets/icon.png`
are committed, so this only needs running if the design changes:

```bash
python assets/make_icon.py assets
```

## Project layout

```
src/main.rs          thin launcher: paths, logging, config, hand off to the UI
build.rs             compiles the icon and version block into the executable
assets/              the application icon, and the script that generates it
crates/
├── mp-core/         settings, paths, colour, and the library:
│                    schema, scanner, queries, playlists, similarity, tags
├── mp-audio/        decode → resample → DSP → output; queue, device, analysis
├── mp-net/          outbound requests, and the log that discloses them
└── mp-ui/           theme, shell, views, widgets, visualizers
```

The split is not ceremony. `mp-core` has no UI or audio dependencies, so it
unit-tests without a window or a sound device; `mp-audio` is free of UI types
for the same reason and can be driven headlessly. A change to a button does not
rebuild the DSP.

`mp-net` is this branch's, and it is where **every** outbound request will live.
Keeping them in one crate is what makes them auditable: if a request could be
made from anywhere else, no amount of reading would tell you what the
application talks to.

It holds the transport, the rate limiter, the on-disk cache, the activity log,
and a registry of every service the build can reach. `ureq` appears exactly
once in the dependency tree, under this crate, which you can check rather than
take on trust:

```bash
cargo tree --workspace -i ureq
```

Fetchers talk to a `Transport` trait rather than to the HTTP client, so misses,
rate limits, dead servers and garbage responses are all tested against a
scripted fake. **No test in the workspace opens a socket.**

## Tests

```bash
cargo test --workspace          # 832 tests
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

Everything runs headless — no window, no sound device — so it all works in CI.

A note on Clippy: it caches per crate, so an incremental run only lints what
changed. Before trusting a clean result, `cargo clean` first.

## Diagnostic examples

Nine examples exist for things unit tests cannot reach. All are headless
unless noted.

**Something will not play, or plays wrong**

```bash
cargo run --release -p mp-audio --example decode_probe -- "C:\path\to\music"
```
Decodes every file under a folder and checks the sample counts survive rate
conversion. No sound device needed.

```bash
cargo run --release -p mp-audio --example playback_check -- "C:\path\to\music"
```
Drives the real engine — play, pause, seek, skip — and asserts the underrun
counter stays at zero. **This one opens the output device and makes noise.**

**Something is missing from the library**

```bash
cargo run --release -p mp-audio --example crossfade_check -- "C:\path	o\music" 4
```
Plays to a real track boundary and asserts a crossfade actually began. Whether
a fade *sounds* right is a matter for ears, but whether one started at all is a
fact, and the two are indistinguishable to a listener. Silent unless you pass
`--audible`.

```bash
cargo run --release -p mp-audio --example silence_probe -- "C:\path	o\music"
```
Reports how much digital silence sits at each end of every file. Answers which
of your tracks the trim setting will actually do anything to: most have none,
and testing on one of those looks exactly like the feature being broken.

```bash
cargo run --release -p mp-core --example library_probe -- "C:\path\to\music"
```
Scans a folder and reports what the library made of it: counts, top artists and
albums, sample rows, and a timed rescan. Writes to a scratch database, so it
never touches your real library.

**Lyrics are not being fetched**

```bash
cargo run -p mp-net --example lyrics_probe -- "Radiohead" "Creep" "Pablo Honey" 239
```
Makes one real request to LRCLIB and prints the URL, what came back and the log
line it produced. The unit tests answer from a scripted fake, which proves the
logic and proves nothing about whether the field names are still right. **This
one uses the network** — it is the only example that does.

**Checking for regressions**

```bash
cargo run --release -p mp-core --example perf_probe -- 30000
```
Fabricates a synthetic library of N tracks and times what every view costs.
Current numbers at 30k: first paint 130 ms, a keystroke 24 ms, a change of sort
68 ms.

```bash
cargo run --release -p mp-audio --example viz_check
```
Proves audio survives the trip from the DSP chain to the visualizer analyzer,
resampler included, and that the reported frequencies are correct.

```bash
cargo run --release -p mp-audio --example preset_headroom
```
Measures the composite peak of every equalizer preset. Run it after adding one:
overlapping bands sum, so a preset's peak is not its loudest slider, and the
preamp has to be measured rather than guessed.

## How it is put together

A few decisions that are load-bearing, and would look arbitrary without the
reason.

**The audio callback is real-time safe.** It never allocates, locks, or does
I/O — it pops from a lock-free ring, applies a smoothed gain, and returns. Every
expensive thing (opening files, building resamplers, computing biquad
coefficients) happens on a worker thread and is shipped over a queue. There are
no transcendental functions in the callback at all; even the limiter's soft knee
is a rational function of the linear peak rather than the usual decibel maths.

**Gapless keeps one stream across track boundaries.** Rather than flushing and
restarting, the next track decodes into the same ring and a bookkeeping type
records the frame where it begins. Position and the now-playing display change
when *playback* crosses the seam, not when decoding does — at a two-second
buffer those are different moments.

**The equalizer curve is derived, not drawn.** The response on screen is
evaluated from the same coefficients the audio thread is running. Measured end
to end, the curve and the sound agree to 0.004 dB.

**Scanning never blocks the interface.** The scan runs on its own thread with
its own connection; WAL mode lets it write while the UI reads. That is a test,
not an aspiration — see `concurrent_scan.rs`.

**A crash cannot corrupt the index.** `wal_crash.rs` re-runs the test binary as
a child process, which commits rows, opens a second transaction, and calls
`abort()` — no unwinding, no cleanup. The parent then asserts the index reopens,
passes an integrity check, kept exactly the committed rows, and still takes
writes.

**Artwork is content-addressed.** Covers are keyed by the hash of their own
bytes and stored pre-resized at 64/256/800 px, so twelve tracks from one album
share one set of files and one GPU texture, and no list decodes a 3000 px cover
to draw a 64 px thumbnail.

**An accent is the most *usable* colour on a cover, not the most common.** The
commonest colour on a sleeve is almost always black, white or a dishwater grey,
so naive "dominant colour" theming produces a grey interface for nearly every
record. Covers are clustered in Oklab and scored on prominence, colourfulness
and lightness together — and a genuinely monochrome sleeve returns *nothing*,
because the configured accent is a better answer than a muddy guess.

**Backgrounds have a ceiling, not a slider to "unreadable".** The album-art and
visualizer panel backgrounds show through at a maximum of 34% however high the
strength is set.

**Theme roles, never literals.** `mp-ui/src/theme.rs` turns a small set of
semantic roles into a complete `egui::Style`; call sites ask for
`palette.text_muted`, never a colour. Body text is held to WCAG AA contrast
automatically, which is what makes adaptive theming safe when the accent could
be any colour a cover happens to contain.

**No bundled fonts or icon fonts.** The UI font is resolved from the system
(Segoe UI Variable → Segoe UI → Inter → egui's built-in). Icons are drawn as
vectors, so they stay crisp at any scale.

**Custom window chrome.** The window is undecorated and draws its own title bar,
because Windows paints the system one from the *system accent colour* and
Windows 10 has no API to override it. Drag, double-click-maximise and edge
resize are reimplemented. Known gap: Windows 11 snap layouts need a
`WM_NCHITTEST` handler winit does not expose.

---

## License

MIT — see [LICENSE](LICENSE).
