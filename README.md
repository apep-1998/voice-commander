# voice-commander

Hold a key, talk, release — and your voice goes wherever you tell it to.

`voice-commander` is a push-to-talk daemon for Linux (Wayland/Hyprland first) that turns a
keybind into a pipeline you define:

```
  keypress ──► record ──► [speech-to-text] ──► callback
                                           ├─► callback
                                           └─► callback
```

**The recording is the only fixed part.** Which speech-to-text engine runs — if any — and what
happens with the result are both configuration, not code. Bind one key to a local Whisper model
that types into the focused window, another to a cloud API that fires three webhooks, and a
third that skips transcription entirely and just hands the `.wav` to a shell script.

## Why

Existing dictation tools hardcode one provider and one action. This one doesn't:

- **Per-keybind profiles.** Every keybind selects its own transcriber and its own set of
  callbacks.
- **Speech-to-text is optional.** A profile can go straight from audio to callbacks.
- **Add providers from a config file.** Generic `http` and `command` adapters cover OpenAI,
  Groq, Deepgram, a local `whisper.cpp`, or your own script — with no recompile.
- **No clipped first syllables.** A daemon holds the microphone open and keeps a rolling
  pre-roll buffer, so the recording starts *before* your keypress landed.
- **Say one more thing.** Release the key, remember something, press again within the cooldown
  window — it continues the same recording instead of starting a new one.
- **Everything is logged.** Timings, audio levels, and per-callback outcomes land in
  structured logs so you can tune the configuration from real usage.

## Status

Early development. See the [PR breakdown](#roadmap) below for what has landed.

## Roadmap

| | Milestone |
|---|---|
| 1 | Workspace scaffolding + CI |
| 2 | Configuration schema and layered loading |
| 3 | Session model + versioned event schema |
| 4 | IPC socket, daemon skeleton, thin client |
| 5 | Audio core: ring buffer, pre-roll, level metering |
| 6 | Real capture backend (`cpal`) + `mic-test` |
| 7 | Session state machine: cooldown, continuation, WAV output |
| 8–9 | Transcribers: `command`, generic `http`, `openai` |
| 10–11 | Callbacks: `command`, `http`, `clipboard`, `type`, `notify`, `file` |
| 12 | Event bus, presenters, `events --follow` |
| 13 | Storage retention, logging, `stats` |
| 14 | Packaging, systemd unit, docs |

Deliberately deferred: the graphical overlay, a native PipeWire backend, and in-process
Whisper. The event stream is a documented, versioned contract precisely so a UI can be built
against it later — by this project or by you.

## License

MIT
