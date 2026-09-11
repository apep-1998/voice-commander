# voice-commander — notes for agents

Push-to-talk voice pipeline for Linux. Hold a key, talk, release; the audio goes through an
optional speech-to-text step and then to any number of user-defined callbacks.

## The one-paragraph version

A resident daemon (`voice-commanderd`) owns the microphone and all session state. A tiny
client (`voice-commander`) is what a Hyprland keybind runs; it sends one line of JSON over a
Unix socket and exits. Keeping the device open is what makes a keypress start capturing
instantly, and a rolling pre-roll buffer means the recording starts *before* the keypress.

## Build, test, lint

```sh
cargo build --workspace
cargo test --workspace --all-features        # must pass with no microphone and no network
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features    # CI runs with -D warnings
cargo test -p vc-audio -- --ignored          # the few tests that need real hardware
```

CI runs all four on every PR. `libasound2-dev` and `pkg-config` are needed to compile `cpal`.

## Crate layout

| Crate | Responsibility |
|---|---|
| `vc-core` | Config schema, session model, event schema, token substitution. **No I/O beyond reading config.** |
| `vc-audio` | `AudioSource` trait, cpal backend, pre-roll ring, level metering, resampling, WAV |
| `vc-exec` | Process execution, timeouts, retry policy — shared by transcribers and sinks |
| `vc-stt` | `Transcriber` trait and adapters |
| `vc-sinks` | `Sink` trait and adapters |
| `vc-ipc` | Socket protocol **and** the blocking client, so tests drive the real client |
| `vc-daemon` | Orchestration: state machine, capture engine, socket server → `voice-commanderd` |
| `vc-cli` | The thin client → `voice-commander` |

## Rules that are not obvious from the code

**`vc-cli` must stay dependency-light.** It runs on every keypress, between the user pressing
a key and audio being captured. No async runtime, no HTTP stack, no config parsing on the
fast path. Currently ~3.3ms per round trip including process spawn. Do not add to it without
measuring.

**The audio callback must never allocate, lock or log.** It writes to a lock-free ring and
nothing else. A missed deadline is a gap in the user's recording. All real work happens on
the capture thread draining that ring.

**No test may require a microphone or a network.** Capture is tested through
`SyntheticSource`; HTTP through a local mock. Hardware tests are `#[ignore]`d. This is a hard
rule — CI has neither.

**The event schema is a public contract.** `docs/events.md` documents it and
`crates/vc-core/tests/event_schema.rs` pins every event's wire format with literal JSON.
Changing a field means bumping `EVENT_SCHEMA_VERSION` and updating the docs. Snapshot
failures are a question ("does this need a version bump?"), not a chore.

**Releasing the key does not end a session.** It opens the cooldown window. Audio captured
during that window is held *aside*, because whether it belongs is not known until the window
closes: press again and it is the pause in the middle of a sentence, let it expire and it is
the user having already stopped talking.

**`max_recording_secs` cannot be zero.** It is the watchdog for a missed release keybind —
release the modifier before the key and the compositor never fires it. Without it the daemon
records until the disk fills.

**Token substitution is per argument, never through a shell.** A transcript is untrusted text
that came out of a microphone. Expanding a whole command line before splitting it would let
`delete everything; rm -rf /` become two commands.

**Secrets have no inline config variant.** `api_key` takes `env` or `command` only. Config
files get committed to dotfile repos and pasted into bug reports.

## Testing style used here

- The session state machine takes **supplied timestamps**, never reads a clock. That is why
  30 of its tests run in 10ms and why "resume 1ms inside the window" is testable at all.
  Do not introduce `sleep` into these.
- Process-level tests spawn **real programs** (`/bin/sh` scripts in a temp dir) rather than
  mocks, because the process boundary is what is being tested.
- Test names are sentences describing the property. Comments say *why the property matters*,
  not what the code does.
- Config tests assert on the **error message**, not just on failure — a validator that
  rejects without saying which key is barely better than one that accepts silently.

## Contributing changes

Stacked PRs: branch `feat/NN-short-name`, each targeting the previous one until merged.
Conventional commit subjects. Every PR body says what it adds, what it deliberately leaves
for later, and how to verify by hand.

Do not commit or push unless asked.

## Where to look first

- `docs/configuration.md` — the user-facing vocabulary, and the best map of the design
- `docs/events.md` — the contract any indicator is built against
- `config/default.toml` — the single source of truth for every default value
- `crates/vc-daemon/src/recorder.rs` — the state machine, the subtlest part of the system
