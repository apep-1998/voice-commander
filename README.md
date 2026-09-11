# voice-commander

Hold a key, talk, release — and your voice goes wherever you tell it to.

`voice-commander` is a push-to-talk daemon for Linux (Wayland and Hyprland first) that turns a
keybind into a pipeline you define:

```
  keypress ──► record ──► [speech-to-text] ──► callback
                                           ├─► callback
                                           └─► callback
```

**The recording is the only fixed part.** Which speech-to-text engine runs — if any — and what
happens with the result are both configuration, not code. Bind one key to a local Whisper
model that types into the focused window, another to a cloud API that fires three webhooks,
and a third that skips transcription entirely and hands the `.wav` to a shell script.

```toml
[profiles.dictate]                    # Super+D — local model, types into the focused window
transcriber = "local"
sinks = ["type_it"]

[profiles.agent]                      # Super+A — cloud model, three callbacks at once
transcriber = "openai"
sinks = ["agent", "archive", "clipboard"]

[profiles.memo]                       # Super+M — no transcription, just the audio file
transcriber = false
sinks = ["archive"]
```

## Why

Existing dictation tools hardcode one provider and one action. This one doesn't.

- **Per-keybind profiles.** Every keybind picks its own transcriber and its own set of
  callbacks.
- **Speech-to-text is optional.** A profile can go straight from audio to callbacks.
- **New providers are a config change.** Generic `http` and `command` adapters cover OpenAI,
  Groq, Deepgram, a local `whisper.cpp`, or your own script — with no recompile.
- **No clipped first syllables.** A resident daemon holds the microphone open and keeps a
  rolling pre-roll buffer, so the recording starts *before* your keypress landed. A 600ms
  hold really does produce 1200ms of audio.
- **Say one more thing.** Release the key, remember something, press again within the cooldown
  window — it continues the same recording instead of starting a new one.
- **Fast.** The client a keybind runs is 920K, has no async runtime, and completes a round
  trip in ~3ms including process spawn.
- **Everything is logged.** Timings, audio levels and per-callback results land in structured
  JSON, so the configuration can be tuned from your own usage rather than from guesswork.

## Quick start

```sh
git clone https://github.com/apep-1998/voice-commander && cd voice-commander
cargo build --release
install -Dm755 target/release/voice-commander{,d} -t ~/.local/bin/

voice-commander mic-test              # check audio actually arrives — do this first
```

Run the daemon:

```sh
install -Dm644 packaging/voice-commander.service ~/.config/systemd/user/
systemctl --user enable --now voice-commander
```

Bind it, in `~/.config/hypr/hyprland.conf`:

```
bind  = SUPER, D, exec, voice-commander start --profile dictate
bindr = SUPER, D, exec, voice-commander stop  --profile dictate
bind  = SUPER, Escape, exec, voice-commander cancel
```

That is enough to record. See **[recipes](docs/recipes.md)** for configurations you can paste
— local dictation, cloud transcription to the clipboard, talking to an AI agent, a voice
journal, and several keybinds at once.

## Documentation

| | |
|---|---|
| **[Installation](docs/installation.md)** | Dependencies, build, systemd, Hyprland/i3 binds, troubleshooting |
| **[Configuration](docs/configuration.md)** | Every setting, what it does, and why it defaults where it does |
| **[Recipes](docs/recipes.md)** | Ten copy-paste configurations |
| **[Events](docs/events.md)** | The stream any indicator is built on — a versioned public contract |

## Commands

```sh
voice-commander start --profile NAME   # begin, or continue a session in its cooldown window
voice-commander stop  --profile NAME   # stop and open the continuation window
voice-commander toggle --profile NAME  # for trigger = "toggle" profiles
voice-commander cancel                 # discard whatever is in flight
voice-commander status                 # what the daemon is doing
voice-commander events --follow        # the event stream, as newline-delimited JSON
voice-commander mic-test               # record briefly and report what arrived
voice-commander reload                 # re-read the configuration
```

## Status

Early, and usable for recording today.

| | |
|---|---|
| Recording — all three capture modes, pre-roll, cooldown continuation, watchdog, cancel | ✅ |
| WAV + `session.json` output with full timings and levels | ✅ |
| `mic-test`, `status`, `events --follow`, `reload` | ✅ |
| `command` transcriber — whisper.cpp, faster-whisper, any script | ✅ |
| `openai` and `http` transcribers | ✅ |
| Callbacks — all six kinds, with fan-out, retries and skip reasons | ✅ |
| `stats`, retention, packaging | planned |

Deliberately deferred: the graphical overlay, a native PipeWire backend, and in-process
Whisper. The event stream is a documented, versioned contract precisely so an indicator can be
built against it later — by this project or by you. There is a working nine-line shell version
at the end of [docs/events.md](docs/events.md).

## Development

See [CLAUDE.md](CLAUDE.md) for the architecture, the invariants that are not obvious from the
code, and the testing conventions.

```sh
cargo test --workspace --all-features   # no microphone or network required
```

## License

MIT
