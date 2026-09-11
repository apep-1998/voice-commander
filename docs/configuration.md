# Configuration reference

Configuration lives in `~/.config/voice-commander/config.toml`, with optional drop-ins in
`~/.config/voice-commander/conf.d/*.toml` applied in filename order.

Everything is optional. Your file is merged **on top of** a built-in baseline, key by key —
so setting one field of `[defaults.capture]` leaves the rest of that section alone. Arrays
replace rather than accumulate, so `sinks = ["a"]` in a drop-in means *these sinks*, not
*these as well as whatever was configured elsewhere*.

Check any edit before relying on it:

```sh
voice-commander config check
```

It reports **every** problem in one pass, with the exact key path — and refuses unknown keys
rather than ignoring them, because a misspelled setting that silently does nothing is the
worst kind of configuration bug.

## The shape of it

```
[daemon]           the daemon itself
[audio]            the format recordings are stored in
[levels]           thresholds that turn signal levels into warnings
[storage]          where recordings go and when they are cleaned up
[feedback]         what you are shown while recording and while callbacks run

[defaults.capture]       inherited by every profile
[defaults.session]
[defaults.continuation]

[transcribers.<name>]    speech-to-text providers — defined once, referenced by name
[sinks.<name>]           callbacks — same
[presenters.<name>]      indicators — same

[profiles.<name>]        what a keybind selects: one capture behaviour,
                         at most one transcriber, any number of callbacks
```

Profiles are the point. Each keybind selects one, and each can use a different transcriber and
a different set of callbacks.

---

## `[defaults.capture]`

Inherited by every profile; any profile can override any field.

```toml
[defaults.capture]
mode = "preroll"
pre_roll_ms = 500
idle_release_secs = 300
device = "default"
```

### `mode`

| Value | Behaviour | Cost |
|---|---|---|
| `"preroll"` *(default)* | Device stays open and a rolling window of recent audio is kept in memory, so the recording starts **before** your keypress | Microphone held open; memory = `pre_roll_ms` of audio |
| `"warm"` | Device stays open, no lookback | Microphone held open |
| `"on_demand"` | Device opens on keypress, closes on release | ~100–200ms of setup during which speech is lost |

`preroll` is why the first syllable of "**o**pen my calendar" survives. Every push-to-talk
tool without it loses that syllable.

### `pre_roll_ms`

How much audio from before the keypress to include. Default `500`, maximum `30000`.

Only meaningful in `preroll` mode. Held in memory continuously and **never written to disk**
unless a recording is actually triggered.

A recording started moments after the daemon opens the device has less lookback than
configured — the buffer has not filled yet. `session.json` records what was *actually*
available, not what was configured.

### `idle_release_secs`

Close the device after this many idle seconds, reopening it on the next keypress. Default
`300`. `0` never closes it.

This is the battery knob: it keeps the zero-latency benefit while you are working and stops
the microphone being held open all night for nothing. The cost is that the first recording
after an idle stretch has no pre-roll.

### `device`

`"default"` follows the system default input, including when it changes. Otherwise any
substring of a device name — `"StreamCam"`, `"Yeti"`. List what is available with
`voice-commander mic-test --device <substring>`, or `wpctl status`.

---

## `[defaults.session]`

```toml
[defaults.session]
cooldown_ms = 1500
max_recording_secs = 120
max_total_secs = 300
```

### `cooldown_ms`

**Releasing the key does not end the recording.** It starts this window. Press again inside it
and you continue the *same* recording — for when you let go and immediately remember one more
thing.

Default `1500`. Raise it if you often find two separate recordings where you wanted one.

`session.json` records `resumed_after_ms` for every continuation. If those cluster just under
your `cooldown_ms`, the window is too short.

### `max_recording_secs`

The watchdog. Default `120`, and it **cannot be zero**.

Release the modifier before the key and your compositor may never fire the release binding.
Without this the daemon would record until the disk filled. A recording that stops for this
reason is recorded as `stop_reason: "watchdog"`.

### `max_total_secs`

Total audio across all segments of one session, however many times it was continued. Default
`300`. Must be at least `max_recording_secs`.

---

## `[defaults.continuation]`

What happens to the pause between two segments of a continued recording.

```toml
[defaults.continuation]
gap = "keep"
silence_ms = 300
max_segments = 5
```

| `gap` | Result |
|---|---|
| `"keep"` *(default)* | The pause is part of the recording — one continuous take, because the device stayed open and captured it anyway |
| `"drop"` | The segments are spliced together and the pause discarded |
| `"silence"` | Spliced with `silence_ms` of digital silence, which nudges some models into treating them as separate utterances |

`"keep"` is impossible in `on_demand` mode — the device is shut during the pause — so it
quietly becomes `"drop"`, and `session.json` records `gap_downgraded_to` so you can see why.

`max_segments` (default `5`) caps how many times one session may be continued.

---

## `[audio]`

```toml
[audio]
sample_rate = 16000
channels = 1
format = "wav"
```

16 kHz mono is what speech models are trained on. Higher rates cost size and buy nothing for
speech. Valid rates: 8000, 16000, 22050, 24000, 44100, 48000.

---

## `[levels]`

Thresholds that turn a raw signal level into a warning you can act on.

```toml
[levels]
silence_dbfs = -55.0
too_quiet_dbfs = -40.0
speech_dbfs = -45.0
silence_warn_after_ms = 1000
```

All values are dBFS, so always negative. Roughly: `-60` is silence, `-40` is a distant or
quiet voice, `-20` to `-12` is a good speaking level, `0` is clipping.

`silence_dbfs` must be below `too_quiet_dbfs` — silence is quieter than merely too quiet.

---

## `[storage]`

```toml
[storage]
# dir = "~/recordings/voice"     # default: $XDG_DATA_HOME/voice-commander
max_age_days = 0                 # 0 = keep forever
max_total_bytes = 0              # 0 = no limit
keep_audio_after_transcribe = true
```

Setting `keep_audio_after_transcribe = false` makes the tool leave far less behind: the
transcript stays, the audio is removed once it has been transcribed.

---

## `[transcribers.<name>]`

Optional. A profile may name one, or set `transcriber = false`, or omit it entirely —
recording straight to callbacks is a first-class case, not a degraded one.

### `type = "command"` — anything with a command line

The offline path. No network, no API key.

```toml
[transcribers.local]
type = "command"
cmd = ["whisper-cli", "-m", "/opt/whisper/ggml-base.en.bin", "-f", "{audio_path}", "-nt"]
text = { from = "stdout" }
timeout_ms = 120000
# env = { OMP_NUM_THREADS = "4" }
```

`cmd` is argv, not a shell string, and `{token}` substitution happens **per argument** — so a
path containing spaces stays one argument and a transcript containing `;` cannot become a
second command. No shell is involved.

The program runs with the session directory as its working directory, so it can write
alongside the recording without being told where that is.

Read the text from a file the program wrote instead:

```toml
text = { from = "file", path = "{session_dir}/out.txt" }
```

### `type = "openai"` — OpenAI and anything that copies its API

```toml
[transcribers.openai]
type = "openai"
model = "gpt-4o-transcribe"
api_key = { env = "OPENAI_API_KEY" }
# api_key = { command = ["pass", "show", "openai/api-key"] }
language = "en"
timeout_ms = 30000
retry = { attempts = 2, backoff_ms = 500 }
```

**The key is never stored in this file.** `api_key` takes `env` or `command` and nothing
else. Config files get committed to dotfile repositories and pasted into bug reports.

### `type = "http"` — any other provider, described field by field

This is how you add a provider nobody has written an integration for.

```toml
[transcribers.groq]
type = "http"
url = "https://api.groq.com/openai/v1/audio/transcriptions"
method = "POST"
headers = { Authorization = "Bearer ${GROQ_API_KEY}" }
audio = { how = "multipart", field = "file" }
form = { model = "whisper-large-v3-turbo" }
response = { format = "json", text_pointer = "/text" }
```

| Field | Meaning |
|---|---|
| `audio.how` | `"multipart"`, `"raw_body"` or `"base64_json"` — providers disagree about this more than anything else |
| `headers` | `${VAR}` is expanded from the environment when the request is built, so rotating a key needs no reload |
| `form` / `json` / `query` | Extra fields, wherever the provider wants them |
| `response.text_pointer` | An RFC 6901 JSON pointer to the transcript, e.g. `/results/channels/0/alternatives/0/transcript` |

---

## `[sinks.<name>]`

What happens with a finished recording. A profile fans out to as many as you like.

Common to every kind:

```toml
requires_text = true       # skip when the session produced no transcript
on_error = "ignore"        # or "fail_session"
timeout_ms = 60000
retry = { attempts = 1, backoff_ms = 500 }
```

`requires_text` defaults to whatever the kind needs — a `clipboard` sink is pointless without
text; a `command` sink handed a `.wav` path is not.

### `type = "command"` — run a program

The general case, and the reason this project exists.

```toml
[sinks.agent]
type = "command"
cmd = ["~/.local/bin/voice-agent.sh", "{text_path}", "{audio_path}"]
timeout_ms = 60000
```

Your program receives, all three ways so you can pick:

- the substituted arguments above
- the same values as `VC_*` environment variables (`$VC_TEXT`, `$VC_AUDIO_PATH`, …)
- the full session metadata as JSON on standard input

### `type = "http"` — POST somewhere

```toml
[sinks.webhook]
type = "http"
url = "https://example.com/hooks/voice"
headers = { Authorization = "Bearer ${WEBHOOK_TOKEN}" }
body = { kind = "session_json" }        # or { kind = "template", template = "…" }
                                        # or { kind = "multipart", audio_field = "file" }
retry = { attempts = 3, backoff_ms = 1000 }
```

### `type = "clipboard"` / `"type"` / `"notify"` / `"file"`

```toml
[sinks.clipboard]
type = "clipboard"
# primary = true        # also the middle-click selection

[sinks.type_it]
type = "type"           # types into the focused window — this is the dictation setup
tool = "auto"           # auto | wtype | ydotool
# key_delay_ms = 0

[sinks.notify]
type = "notify"
summary = "voice-commander"
body = "{text}"

[sinks.journal]
type = "file"
path = "~/notes/voice/{date}.md"
template = "- {started_at} {text}\n"
```

### Tokens

Available in `cmd`, `path`, `template`, and HTTP body templates:

| Token | |
|---|---|
| `{audio_path}` | the `.wav` |
| `{text_path}` | the transcript file, empty when there is no transcript |
| `{text}` | the transcript itself |
| `{session_id}` | e.g. `20260911T144812Z-dictate-2hc8b` |
| `{session_dir}` | the directory holding both files |
| `{profile}` | which profile recorded it |
| `{duration_ms}` | length of the audio |
| `{started_at}` | RFC 3339 |
| `{date}` | `2026-09-11` |
| `{language}` | when the transcriber reports one |

Unknown tokens are left exactly as written rather than blanked, so a JSON template containing
braces is safe. `config check` warns about anything that looks like a typo.

---

## `[profiles.<name>]`

```toml
[profiles.dictate]
trigger = "push_to_talk"        # or "toggle"
transcriber = "local"           # a name, or false for no transcription
sinks = ["type_it"]
sink_mode = "parallel"          # or "sequential"

# Override any inherited section, key by key:
capture = { pre_roll_ms = 300 }
session = { cooldown_ms = 3000 }
```

`sink_mode = "parallel"` is the default: callbacks are independent, so the total wait is the
slowest one rather than the sum of all of them. Use `"sequential"` when a later callback
depends on what an earlier one did.

A profile with no transcriber and no sinks is legitimate — the recording is stored either
way, so "just archive my voice" is a valid profile.

---

## `[feedback]` and `[presenters.<name>]`

```toml
[feedback]
enabled = true
listening = true            # events while you are speaking
processing = true           # events while callbacks run
processing_linger_ms = 1500
level_interval_ms = 50      # 20 level updates a second
presenters = ["socket"]

[presenters.socket]
type = "socket"
```

**No graphical indicator ships yet.** What ships is the event stream one would be built on:

```sh
voice-commander events --follow
```

`listening` and `processing` are independent switches, so you can have one without the other.
See [events.md](events.md) to build your own — it is a documented, versioned contract, and a
working nine-line shell indicator is at the bottom of that file.

---

## What is implemented today

| | |
|---|---|
| Recording, all three capture modes, pre-roll | ✅ |
| Cooldown continuation and all three gap modes | ✅ |
| Watchdog, cancel, toggle | ✅ |
| WAV and `session.json` output | ✅ |
| `mic-test`, `status`, `events --follow`, `reload` | ✅ |
| `command` transcriber | ✅ |
| `openai` and `http` transcribers | ✅ |
| Callbacks — all six kinds, with fan-out, retries and skip reasons | ✅ |
| `stats`, retention, `config init`, packaging | ✅ |
| Graphical indicator | deliberately deferred — build one on the event stream |

Configuring something not yet implemented is reported by name at startup rather than silently
doing nothing.
