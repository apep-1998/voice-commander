# The overlay

`voice-commander-overlay` is the on-screen indicator: a ring that shows your voice arriving
while you speak, and a panel that shows each callback resolving afterwards.

```sh
voice-commander-overlay --demo      # a scripted session, no microphone needed
voice-commander-overlay             # the real thing
```

Add `exec-once = voice-commander-overlay` to your Hyprland config to have it always there.
It costs **0.2% of a core while nothing is happening**, so leaving it running is free.

---

## Changing it

Everything lives under `[overlay]` in `~/.config/voice-commander/config.toml`. **Every field
is optional** — set only what you want to change:

```toml
[overlay]
position = "centre"

[overlay.colours]
hud = "#ff9de2"
```

Check your edit before relying on it:

```sh
voice-commander config check
```

A mistyped colour or a nonsensical frame rate is reported by name, with the key path.

Command-line flags override the file for one run, which is how to try something without
editing anything:

```sh
voice-commander-overlay --demo --position centre --size 200 --fps 30
```

---

## Where and how big

```toml
[overlay]
position = "bottom"     # bottom | top | centre | bottom_right | bottom_left
margin = 72             # distance from that screen edge; ignored when centred
size = 240              # diameter of the ring, in pixels
fps = 60                # frames per second while something is on screen
font = "Chakra Petch"   # falls back to whatever the system has
```

`bottom` is the default because dictation types into whatever window has focus, and an
indicator in the middle of the screen sits on top of what you are dictating into.

**`fps` is the main thing deciding what it costs.** The ring is drawn in software:

| | |
|---|---|
| Nothing on screen | 0.2% of a core |
| Visible at `fps = 60` | ~30% |
| Visible at `fps = 30` | ~15% |

30 is smooth for a level meter. Everything scales with `size`, so a smaller ring is also a
cheaper one.

For the intended typography: `pacman -S ttf-chakra-petch`. Without it the text falls back to
your system sans — legible, but not the angular face the design is built around.

---

## How calm it is

```toml
[overlay.motion]
wave_speed = 0.55         # radians/sec the swell travels when silent
wave_speed_voice = 3.4    # radians/sec at full voice
attack = 0.28             # how fast the ring rises to your voice, 0..1 per frame
release = 0.04            # how slowly it subsides
arc_speed = 1.0           # rotation of the decorative arcs. 0 stops them.
quiet_enter_dbfs = -48.0  # below this, the input is reported as too quiet
quiet_leave_dbfs = -42.0  # above this, it is reported as fine again
```

These are the settings worth understanding, because the first version of this overlay got
them wrong in a way that was accurate and horrible to sit under.

**The wave speeds up when you speak.** Height alone says how loud you were; speed says
*something is happening right now*, which is the part that reads at a glance. It drifts
gently at `wave_speed` when silent and travels at `wave_speed_voice` at full voice, following
the same envelope as the height. Set them to the same number for a constant drift.

**`attack` and `release` are an envelope follower.** The ring does not show each level
measurement — twenty of those a second makes it jitter and constantly pull your eye. It shows
a swell that rises quickly when you start talking (`attack`) and subsides slowly when you
stop (`release`), so it settles like water rather than snapping shut between syllables. Keep
`release` much smaller than `attack`; `config check` warns if you invert them.

**The two `quiet_*` thresholds are deliberate.** A single threshold makes the whole instrument
flip colour every time ordinary speech dips between words. With two, the input has to
genuinely get quieter to turn amber and genuinely recover to turn back — and between them,
whatever state it is in simply holds.

To make it calmer still:

```toml
[overlay.motion]
wave_speed = 0.3
wave_speed_voice = 1.2   # less of a surge when you speak
arc_speed = 0.0          # stop the rotating arcs entirely
release = 0.02           # even slower to subside
```

---

## Colour

```toml
[overlay.colours]
hud = "#58d7ff"           # the primary instrument colour
deep = "#2b93b8"          # cardinal graduations, secondary arcs
dim = "#1c5a72"           # inactive ticks, and the swell when nothing is said
rule = "#14293a"          # frame lines
warn = "#ffb64d"          # input too quiet
critical = "#ff5f56"      # clipping, and failed callbacks
ok = "#6fe3a8"            # succeeded callbacks
text = "#cfe9f5"
text_dim = "#6f93a6"
ground = "#05080cd2"      # behind the ring — the trailing d2 is opacity
panel_ground = "#060a0f"  # behind the callback list
```

`#rrggbb`, or `#rrggbbaa` to include opacity. `ground` is translucent on purpose: a heads-up
display floats over the desktop rather than covering it. `panel_ground` is opaque on purpose:
it carries 13px text, and a bright window showing through makes that unreadable.

The default cyan is `#58d7ff` rather than `#00ffff` — pure cyan reads as generic neon, and
pulled toward sky and desaturated it reads as an instrument.

---

## Shape

```toml
[overlay.geometry]
wave_base = 68      # radius the swell oscillates about
wave_height = 16    # how far it rises at full voice
panel_width = 300   # width of the callback list
```

Both wave figures are for a 240px ring and scale with `size`.

Keep `wave_height` well under half of `wave_base`. Given a large amplitude the swell folds in
over the readout and reads as a blob rather than a wave; `config check` warns before you find
that out by looking.

---

## What it draws, and when

Every frame comes from the event stream in [events.md](events.md) — the same lines
`voice-commander events --follow` prints. The overlay has no privileged access to the daemon,
which is the point: anything else can be built the same way.

| Event | On screen |
|---|---|
| `recording_started` | The ring appears, with how much lookback was captured |
| `level` | Feeds the swell, and decides the colour |
| `recording_stopped` with `reason: watchdog` | Red, saying the release keybind did not fire |
| `cooldown_started` | A countdown arc, and "say more?" — it does **not** hide |
| `recording_resumed` | Back to listening, naming the segment |
| `pipeline_started` | The panel opens with every callback listed and dimmed |
| `sink_finished` | That row resolves: `+` green, `x` red with the reason, `-` grey with the skip reason |
| `pipeline_finished` | A summary, then it fades out |

Turning feedback off in `[feedback]` stops the events at the source, so the overlay simply
never appears — there is no separate switch for it.

---

## Anything not listed here

The remaining constants live in `crates/vc-overlay/src/theme.rs`, grouped so they read as a
set: tick radii, arc radii and their individual speeds, bracket size. They are not
configuration because changing them is changing the design rather than tuning it. If you find
yourself wanting one of them exposed, it probably should be.
