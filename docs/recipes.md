# Recipes

Working configurations to copy. Each is self-contained — paste one into
`~/.config/voice-commander/config.toml`, adjust the paths, and run
`voice-commander config check`.

Every recipe pairs a config block with the keybind that drives it.

---

## 1. Just record me

No transcription, no callbacks. Hold a key, talk, and the audio is filed away with full
metadata. Nothing to install, no API key, works immediately.

```toml
[defaults.capture]
mode = "preroll"
pre_roll_ms = 500

[profiles.memo]
transcriber = false
sinks = []
```

```
bind  = SUPER, M, exec, voice-commander start --profile memo
bindr = SUPER, M, exec, voice-commander stop  --profile memo
```

Recordings land in `~/.local/share/voice-commander/recordings/YYYY/MM/DD/`.

**Start here.** Confirm recording works before adding anything that can fail.

---

## 2. Hand the audio to a script

The simplest useful setup, and the one everything else is a variation of. Your script gets the
`.wav` path and can do whatever it likes.

```toml
[sinks.my_script]
type = "command"
requires_text = false
cmd = ["~/.local/bin/on-recording.sh", "{audio_path}"]
timeout_ms = 60000

[profiles.memo]
transcriber = false
sinks = ["my_script"]
```

```sh
#!/bin/sh
# ~/.local/bin/on-recording.sh
# $1 is the .wav. $VC_SESSION_ID, $VC_DURATION_MS and friends are also set,
# and the full session JSON arrives on stdin.
cp "$1" ~/Dropbox/voice/
notify-send "Recorded" "$(basename "$1")"
```

---

## 3. Local dictation — types into whatever window has focus

Fully offline. Needs [whisper.cpp](https://github.com/ggerganov/whisper.cpp) and `wtype`.

```sh
sudo pacman -S wtype                      # or: apt install wtype
# build whisper.cpp and fetch a model, e.g. ggml-base.en.bin
```

```toml
[transcribers.local]
type = "command"
cmd = [
  "whisper-cli",
  "-m", "/opt/whisper/ggml-base.en.bin",
  "-f", "{audio_path}",
  "-nt",                                  # no timestamps, just the text
]
text = { from = "stdout" }
timeout_ms = 120000

[sinks.type_it]
type = "type"
tool = "auto"

[profiles.dictate]
transcriber = "local"
sinks = ["type_it"]
capture = { pre_roll_ms = 300 }
session = { cooldown_ms = 800 }
```

```
bind  = SUPER, D, exec, voice-commander start --profile dictate
bindr = SUPER, D, exec, voice-commander stop  --profile dictate
```

A short `cooldown_ms` suits dictation: you want the text to appear promptly, and you are
unlikely to want to continue a sentence after letting go.

---

## 4. Cloud transcription to the clipboard

Accurate, and the result is one paste away.

```sh
# Put the key somewhere the config does not have to hold it:
echo 'export OPENAI_API_KEY=sk-…' >> ~/.config/environment.d/openai.conf
# or use a password manager — see below
```

```toml
[transcribers.openai]
type = "openai"
model = "gpt-4o-transcribe"
api_key = { env = "OPENAI_API_KEY" }
language = "en"

[sinks.clipboard]
type = "clipboard"

[sinks.notify]
type = "notify"
summary = "Transcribed"
body = "{text}"

[profiles.clip]
transcriber = "openai"
sinks = ["clipboard", "notify"]
```

```
bind  = SUPER, C, exec, voice-commander start --profile clip
bindr = SUPER, C, exec, voice-commander stop  --profile clip
```

Keeping the key out of the config file, using a password manager:

```toml
api_key = { command = ["pass", "show", "openai/api-key"] }
```

---

## 5. Talk to an AI agent

The case this project was built for: speak, and a program receives what you said.

```toml
[transcribers.openai]
type = "openai"
api_key = { env = "OPENAI_API_KEY" }

[sinks.agent]
type = "command"
cmd = ["~/.local/bin/voice-agent.sh", "{text_path}"]
timeout_ms = 120000

[sinks.archive]
type = "command"
requires_text = false
cmd = ["~/.local/bin/archive.sh", "{audio_path}", "{text_path}"]

[profiles.agent]
transcriber = "openai"
sinks = ["agent", "archive"]
sink_mode = "parallel"
session = { cooldown_ms = 3000 }
```

```sh
#!/bin/sh
# ~/.local/bin/voice-agent.sh
prompt=$(cat "$1")
reply=$(claude -p "$prompt")              # or any CLI you like
notify-send "Reply" "$reply"
printf '%s' "$reply" | wl-copy
```

A long `cooldown_ms` suits this: you are composing a request, and pausing to think should not
split it into two.

Both callbacks run at once, so the total wait is the slower of them rather than the sum.

---

## 6. A voice journal

Appends to a dated Markdown file. Pairs well with Obsidian or plain notes.

```toml
[transcribers.local]
type = "command"
cmd = ["whisper-cli", "-m", "/opt/whisper/ggml-base.en.bin", "-f", "{audio_path}", "-nt"]
text = { from = "stdout" }

[sinks.journal]
type = "file"
path = "~/notes/voice/{date}.md"
template = "- **{started_at}** {text}\n"

[profiles.journal]
trigger = "toggle"
transcriber = "local"
sinks = ["journal"]
session = { max_recording_secs = 900, max_total_secs = 1800 }
```

```
bind = SUPER_SHIFT, J, exec, voice-commander toggle --profile journal
```

`trigger = "toggle"` means one keybind starts and the same keybind stops, which is what you
want for anything longer than a sentence — holding a key for ten minutes is not a plan.

---

## 7. Several keybinds, several behaviours

The whole point of profiles. One config, four keys, four different pipelines.

```toml
# ── speech-to-text providers ───────────────────────────────────────────────
[transcribers.fast]
type = "command"
cmd = ["whisper-cli", "-m", "/opt/whisper/ggml-tiny.en.bin", "-f", "{audio_path}", "-nt"]
text = { from = "stdout" }

[transcribers.accurate]
type = "openai"
model = "gpt-4o-transcribe"
api_key = { env = "OPENAI_API_KEY" }

# ── callbacks ──────────────────────────────────────────────────────────────
[sinks.type_it]
type = "type"

[sinks.clipboard]
type = "clipboard"

[sinks.agent]
type = "command"
cmd = ["~/.local/bin/voice-agent.sh", "{text_path}"]
timeout_ms = 120000

[sinks.archive]
type = "command"
requires_text = false
cmd = ["~/.local/bin/archive.sh", "{audio_path}"]

# ── profiles ───────────────────────────────────────────────────────────────
[profiles.dictate]                    # Super+D — local, fast, types it
transcriber = "fast"
sinks = ["type_it"]
capture = { pre_roll_ms = 300 }
session = { cooldown_ms = 800 }

[profiles.clip]                       # Super+C — accurate, to the clipboard
transcriber = "accurate"
sinks = ["clipboard"]

[profiles.agent]                      # Super+A — accurate, to an agent, archived
transcriber = "accurate"
sinks = ["agent", "archive"]
session = { cooldown_ms = 3000 }

[profiles.memo]                       # Super+M — no transcription at all
transcriber = false
sinks = ["archive"]
```

```
bind  = SUPER, D, exec, voice-commander start --profile dictate
bindr = SUPER, D, exec, voice-commander stop  --profile dictate
bind  = SUPER, C, exec, voice-commander start --profile clip
bindr = SUPER, C, exec, voice-commander stop  --profile clip
bind  = SUPER, A, exec, voice-commander start --profile agent
bindr = SUPER, A, exec, voice-commander stop  --profile agent
bind  = SUPER, M, exec, voice-commander start --profile memo
bindr = SUPER, M, exec, voice-commander stop  --profile memo
bind  = SUPER, Escape, exec, voice-commander cancel
```

---

## 8. A provider nobody has integrated

Groq, via the generic `http` adapter. Fast and cheap, OpenAI-compatible.

```toml
[transcribers.groq]
type = "http"
url = "https://api.groq.com/openai/v1/audio/transcriptions"
method = "POST"
headers = { Authorization = "Bearer ${GROQ_API_KEY}" }
audio = { how = "multipart", field = "file" }
form = { model = "whisper-large-v3-turbo", response_format = "json" }
response = { format = "json", text_pointer = "/text" }
timeout_ms = 30000
retry = { attempts = 2, backoff_ms = 500 }
```

Deepgram wants the raw bytes as the body and puts the transcript somewhere else entirely.
Same adapter, different description:

```toml
[transcribers.deepgram]
type = "http"
url = "https://api.deepgram.com/v1/listen"
headers = { Authorization = "Token ${DEEPGRAM_API_KEY}", "Content-Type" = "audio/wav" }
audio = { how = "raw_body" }
query = { model = "nova-2", smart_format = "true" }
response = { format = "json", text_pointer = "/results/channels/0/alternatives/0/transcript" }
```

A local `whisper.cpp` server, if you would rather run one than shell out per recording:

```toml
[transcribers.local_server]
type = "http"
url = "http://127.0.0.1:8080/inference"
audio = { how = "multipart", field = "file" }
form = { response_format = "json" }
response = { format = "json", text_pointer = "/text" }
```

---

## 9. Save power on a laptop

Keep the latency benefit while you are working; let go of the microphone when you are not.

```toml
[defaults.capture]
mode = "preroll"
pre_roll_ms = 500
idle_release_secs = 120     # close the device after two idle minutes
```

The first recording after an idle stretch has no pre-roll — the buffer has not refilled — and
`session.json` records that honestly. Every recording after it does.

For the lowest possible idle draw, at the cost of losing the first word every time:

```toml
[defaults.capture]
mode = "on_demand"
pre_roll_ms = 0
idle_release_secs = 0
[defaults.continuation]
gap = "drop"                # "keep" is impossible when the device is shut
```

---

## 10. A bar indicator, in nine lines

There is no graphical indicator yet — deliberately. The event stream is the contract, and
anything can consume it.

```sh
#!/bin/sh
# ~/.local/bin/voice-indicator.sh — a waybar custom module
voice-commander events --follow | while read -r line; do
  case "$(printf '%s' "$line" | jq -r .event)" in
    recording_started)  printf '🔴 listening\n' ;;
    level)              printf '🔴 %s dB\n' "$(printf '%s' "$line" | jq -r '.rms_dbfs|round')" ;;
    session_finalized)  printf '⏳ processing\n' ;;
    pipeline_finished)  printf '\n' ;;
  esac
done
```

```jsonc
// ~/.config/waybar/config
"custom/voice": {
  "exec": "~/.local/bin/voice-indicator.sh",
  "format": "{}"
}
```

For something you can run right now that shows the whole thing — a live level meter and a
per-callback progress list — see `examples/terminal-indicator.sh`:

```sh
./examples/terminal-indicator.sh
```

See [events.md](events.md) for everything available, including the per-callback progress
events a richer indicator uses.

---

## Tuning it from real use

After a couple of weeks, your own recordings will tell you what the settings should be:

```sh
# How often did you continue a recording, and how close to the deadline?
jq -r '.segments[] | select(.resumed_after_ms) | .resumed_after_ms' \
  ~/.local/share/voice-commander/recordings/*/*/*/*/session.json | sort -n | tail

# How much speech landed in the pre-roll window — i.e. how much you would have lost?
jq -r '.segments[0].speech_in_pre_roll_ms' \
  ~/.local/share/voice-commander/recordings/*/*/*/*/session.json | sort -n | tail

# Any recordings cut off by the watchdog? That means a release keybind is not firing.
jq -r 'select(.segments[].stop_reason == "watchdog") | .id' \
  ~/.local/share/voice-commander/recordings/*/*/*/*/session.json
```

- `resumed_after_ms` clustering just under your `cooldown_ms` → the window is too short.
- `speech_in_pre_roll_ms` near your `pre_roll_ms` → raise it, speech is still being clipped.
- `speech_in_pre_roll_ms` consistently `0` → lower it, it is only costing memory.

Or just run `voice-commander stats`, which does all of this and says what it thinks:

```console
$ voice-commander stats
214 recordings
  total audio:   38.2 minutes
  typical length: 4.1s (longest 47.0s)
  continued:     31 (14%)
     you press again within 1420ms, 90% of the time
  speech caught before the keypress: up to 480ms (window is 500ms)
  transcribed:   214 (3 failed, typically 780ms)
  callbacks:     428 run, 2 failed

suggestions:
  capture.pre_roll_ms: speech regularly fills the whole 500ms pre-roll window
    (90th percentile 480ms) — words are probably still being clipped; try 1000ms
  session.cooldown_ms: continuations arrive up to 1420ms after release
    (90th percentile of 31) — a cooldown_ms comfortably above that catches them all
```

It stays quiet until there are at least ten recordings: advice from four is noise wearing a
suit.
