# The event stream

**Schema version: 1**

voice-commander ships no graphical indicator. What it ships is this stream — and the
deliberate consequence is that an overlay written later has no more access to the daemon
than a twenty-line shell script piping events into a status bar. Neither is privileged.

If you want to build an indicator, everything you need is here.

## Reading it

```sh
voice-commander events --follow           # newline-delimited JSON on stdout
voice-commander events --follow | jq -c 'select(.event == "level")'
```

Every session's events are also appended to
`$XDG_DATA_HOME/voice-commander/events.jsonl`, so the same format works for after-the-fact
analysis.

## Envelope

Every line is one JSON object:

```json
{"v":1,"ts":"2026-09-11T14:48:12Z","session":"20260911T144812Z-dictate-2hc8b","profile":"dictate","event":"recording_started","segment":0,"pre_roll_ms":500,"device":"Wireless Microphone RX"}
```

| Field | Always present | Meaning |
|---|---|---|
| `v` | yes | Schema version. Refuse to guess if you do not recognise it. |
| `ts` | yes | RFC 3339, UTC. |
| `session` | no | Omitted — not `null` — for daemon-wide events. |
| `profile` | no | Omitted for daemon-wide events. |
| `event` | yes | Which event this is. The payload fields sit alongside it, not nested. |

Payload fields are flattened into the same object, so `jq 'select(.event == "level") | .rms_dbfs'`
works without reaching through a wrapper.

## Compatibility

Within a schema version, the daemon may **add** a new event kind or a new optional field.
Consumers must ignore event kinds and fields they do not know.

Anything else — renaming a field, removing one, changing a type or the meaning of a value —
requires bumping `v` and a note in this file. `crates/vc-core/tests/event_schema.rs` pins the
exact wire format of every event with literal JSON, so this cannot happen by accident.

## Building a listening indicator

Shown while the user is speaking; gone when they stop. Enabled by `feedback.listening`.

```
recording_started ──► level ──► level ──► … ──► recording_stopped
                        ▲                              │
                        │                       cooldown_started
                 input_warning                         │
                                            ┌──────────┴──────────┐
                              recording_resumed            (window expires)
                                     │                            │
                                     └──► level … ──►      session_finalized
```

The important detail: **`recording_stopped` is not the end.** Releasing the key opens the
continuation window, and pressing again inside it resumes the *same* session. An indicator
that hides on `recording_stopped` will flicker every time the user pauses to think. Hide on
`session_finalized`, or dim during `cooldown_started` and restore on `recording_resumed`.

### `recording_started`

```json
{"event":"recording_started","segment":0,"pre_roll_ms":500,"device":"Wireless Microphone RX"}
```

`pre_roll_ms` is how much audio from *before* the keypress was actually available, which is
not necessarily what was configured — the buffer may not have been full yet.

### `level`

```json
{"event":"level","rms_dbfs":-32.5,"peak_dbfs":-12.0,"speech":true,"clipping":false}
```

Emitted only while recording, throttled to `feedback.level_interval_ms` (default 50ms, so
twenty per second). `speech` is `true` when the signal is above the speech threshold rather
than being room tone — which is how an indicator can show *your voice is arriving*, not
merely *the microphone is on*.

Both figures are dBFS, so always negative. Roughly: `-60` is silence, `-40` is a distant or
quiet voice, `-20` to `-12` is a good speaking level, `0` is clipping.

### `input_warning`

```json
{"event":"input_warning","kind":"device_muted","detail":"source 47 is muted"}
```

`kind` is one of `no_device`, `device_muted`, `silence`, `too_quiet`, `clipping`, `xrun`.
`detail` is optional and human-readable. These are the messages worth interrupting a user
with: `device_muted` in particular is the single most common cause of an empty recording,
and without it the user sees a flat meter and has to work out why themselves.

### `recording_stopped`

```json
{"event":"recording_stopped","segment":0,"duration_ms":2400,"reason":"released"}
```

`reason` is one of `released`, `watchdog`, `total_limit`, `segment_limit`, `cancelled`,
`shutdown`. Worth showing `watchdog` differently: it means a release keybind never fired —
usually because the modifier was released before the key — and the user needs to fix a
binding, not their microphone.

### `cooldown_started` / `recording_resumed`

```json
{"event":"cooldown_started","ms":1500}
{"event":"recording_resumed","segment":1,"resumed_after_ms":1420}
```

`resumed_after_ms` is how long after the release the user pressed again. When these cluster
just under the configured `cooldown_ms`, the window is too short and continuations are being
missed.

### `session_finalized`

```json
{"event":"session_finalized","audio_path":"/home/u/.local/share/voice-commander/recordings/…/audio.wav","total_ms":4200,"segments":2}
```

The audio is written. A listening indicator should disappear here.

## Building a processing indicator

Shown while callbacks run. Enabled by `feedback.processing`.

```
pipeline_started  ← the complete plan, before anything runs
      │
      ├─ transcribe_started ──► transcribe_done | transcribe_failed
      │
      ├─ sink_started(0) ──► sink_finished(0)      ← rows resolve independently
      ├─ sink_started(1) ──► sink_finished(1)
      └─ sink_started(2) ──► sink_finished(2)
      │
pipeline_finished  ← close after feedback.processing_linger_ms
```

### `pipeline_started`

```json
{"event":"pipeline_started","transcriber":"openai","sinks":[
  {"id":0,"name":"agent","kind":"command","requires_text":true},
  {"id":1,"name":"archive","kind":"command","requires_text":false},
  {"id":2,"name":"clipboard","kind":"clipboard","requires_text":true}
]}
```

**This event carries the entire plan up front, and that is its purpose.** A progress list has
to be drawn complete and greyed out before the first callback starts; announcing each sink as
it began would give a list that grows while the user watches it.

`transcriber` is omitted when the profile has none. `requires_text` tells you a row is
conditional, so you can mark it as such from the beginning.

### `sink_started` / `sink_finished`

```json
{"event":"sink_started","id":0,"name":"agent"}
{"event":"sink_finished","id":0,"name":"agent","outcome":{"status":"ok"},"latency_ms":812,"attempts":1}
```

Correlate on `id`, not `name` — the same sink may be listed twice in a profile, and two rows
sharing an identifier cannot be told apart.

`outcome` is a tagged union with exactly three shapes, which map onto exactly three ways to
draw a row:

```json
{"status":"ok"}                                          → green tick
{"status":"failed","error":"connection refused"}         → red cross, with a reason
{"status":"skipped","reason":"no_transcript"}            → greyed out, with a reason
```

`reason` is one of `no_transcript`, `transcription_failed`, `earlier_sink_failed`,
`cancelled`. A greyed row without a reason is indistinguishable from one that silently never
ran, which is why the reason is mandatory.

### `pipeline_finished`

```json
{"event":"pipeline_finished","ok":2,"failed":1,"skipped":0,"total_ms":31200,"outcome":"partial"}
```

Deliberately self-contained: a consumer that connected late can render a final state without
having seen any of the preceding events. `outcome` is `ok`, `partial`, `failed` or
`cancelled`.

Close the panel `feedback.processing_linger_ms` after this — long enough to read, short
enough not to be in the way.

## Everything else

| Event | Payload | Notes |
|---|---|---|
| `daemon_ready` | `version`, `socket` | First event on the stream. |
| `config_reloaded` | `warnings` | Count of warnings; a non-zero value is worth surfacing. |
| `device_opened` | `device`, `sample_rate`, `channels` | The format actually negotiated, which may not be what was asked for. |
| `device_closed` | `reason` | `idle`, `recording_ended`, `lost`, `shutdown`. |
| `session_cancelled` | `reason` | Nothing downstream will run. |
| `transcribe_started` | `transcriber` | |
| `transcribe_done` | `chars`, `latency_ms`, `language?` | |
| `transcribe_failed` | `error`, `latency_ms` | The latency of a *failure* is what tells a user to lower a timeout. |
| `error` | `stage`, `message` | `stage` is `capture`, `storage`, `transcription`, `sink` or `config`. Belongs to neither indicator, so disabling both must not hide it. |

## A worked example

`examples/terminal-indicator.sh` is a complete one — a live level meter while you speak, and a
per-callback progress list while the pipeline runs, in about a hundred lines of shell. It is
worth reading before writing a graphical one, because it does everything an overlay would have
to do and has no more access to the daemon than you do:

```
* listening  (500ms of lookback)
  ########............  -32.4 dBFS
  stopped after 875ms (released)
  ... 800ms to say more
  saved 0.9s of audio
  transcribing with openai...
  o agent (command)          <- the whole plan, drawn before anything runs
  o archive (command)
  o clipboard (clipboard)
  + transcribed 27 chars in 780ms
  + clipboard  3ms           <- and each row resolving as it finishes
  x archive  webhook unreachable
  + agent  405ms
  2 done, 1 failed, 0 skipped  (909ms)
```

A minimal bar indicator, in about as much code as it takes to read this sentence:

```sh
#!/bin/sh
voice-commander events --follow | while read -r line; do
  case "$(printf '%s' "$line" | jq -r .event)" in
    recording_started)  printf '🔴 listening\n' ;;
    level)              printf '🔴 %s dBFS\n' "$(printf '%s' "$line" | jq -r '.rms_dbfs | round')" ;;
    session_finalized)  printf '⏳ processing\n' ;;
    pipeline_finished)  printf '\n' ;;
  esac
done
```
