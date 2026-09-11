# Installing voice-commander

## What you need

| | |
|---|---|
| OS | Linux. Wayland is the target; X11 works for everything except the future overlay. |
| Audio | PipeWire (recommended) or PulseAudio or bare ALSA |
| Build | Rust 1.85 or newer, and ALSA development headers |

On Arch:

```sh
sudo pacman -S rust alsa-lib pkgconf
```

On Debian or Ubuntu:

```sh
sudo apt install cargo libasound2-dev pkg-config
```

## Build and install

```sh
git clone https://github.com/apep-1998/voice-commander
cd voice-commander
cargo build --release
install -Dm755 target/release/voice-commander  ~/.local/bin/voice-commander
install -Dm755 target/release/voice-commanderd ~/.local/bin/voice-commanderd
```

Make sure `~/.local/bin` is on your `PATH`. Two binaries are installed:

- **`voice-commanderd`** — the daemon. It holds the microphone, the pre-roll buffer and all
  session state. It runs all the time.
- **`voice-commander`** — the client your keybind runs. It sends one message to the daemon
  and exits, in about 3ms.

## Check your microphone first

Before configuring anything, confirm audio actually reaches the machine:

```sh
voice-commander mic-test
```

```
recording for 3s — say something...

  device:  Logitech StreamCam
  format:  16000 Hz, 1 channel(s)
  peak:    -11.8 dBFS
  average: -32.1 dBFS
  speech:  2.8s of 3.0s

microphone looks good — -32 dBFS average, -12 dBFS peak
```

This runs **without the daemon**, on purpose — it is what you reach for when recording
produces nothing, and it would be useless if it needed the thing that is not working.

If it says the microphone is silent, check `wpctl status` for which source is default and
whether it is muted. That is the most common cause of an empty recording by a wide margin.

To test a specific input: `voice-commander mic-test --device StreamCam` (any substring of the
device name).

## Create a configuration

```sh
voice-commander config init          # writes a commented starter file
voice-commander config check         # validate it
```

Everything works with no configuration at all — the built-in defaults record and archive.
See [configuration.md](configuration.md) for the full reference and
[recipes.md](recipes.md) for setups you can copy.

Validate any edit before relying on it — it reports every problem in one pass, with the exact
key path:

```sh
voice-commander config check
voice-commander config show          # the fully resolved configuration, defaults filled in
```

## Run the daemon

### As a systemd user service (recommended)

```sh
install -Dm644 packaging/voice-commander.service \
  ~/.config/systemd/user/voice-commander.service
systemctl --user daemon-reload
systemctl --user enable --now voice-commander
```

Check it:

```sh
systemctl --user status voice-commander
journalctl --user -u voice-commander -f
voice-commander status
```

### By hand, while you are setting things up

```sh
voice-commanderd --log debug
```

### Why not socket activation

The daemon exists so that pressing a key starts capturing audio *immediately* — the device is
already open and the pre-roll buffer already holds the last half second. Starting it on demand
reintroduces exactly the latency it removes. Run it as a service.

## Bind it

### Hyprland

Add to `~/.config/hypr/hyprland.conf`:

```
# Hold Super+R to talk, release to stop.
bind  = SUPER, R, exec, voice-commander start --profile dictate
bindr = SUPER, R, exec, voice-commander stop  --profile dictate

# Escape throws away whatever is in flight.
bind  = SUPER, Escape, exec, voice-commander cancel
```

If your config binds by keycode (`bind = SUPER, code:27, …`), use that style — `code:27` is
`r` on a US layout, and keycodes keep working when you switch layouts.

**One thing to know:** `bindr` fires on release of the key, but if you let go of `SUPER`
*before* `R`, Hyprland may never deliver it. Rather than lose the recording, the daemon has a
watchdog — `max_recording_secs`, 120 by default — that stops it regardless. If you notice
recordings that are exactly that long, that is what happened.

For a profile with `trigger = "toggle"` you only need one binding:

```
bind = SUPER_SHIFT, R, exec, voice-commander toggle --profile meeting
```

### i3 or sway

```
bindsym --release $mod+r exec voice-commander stop  --profile dictate
bindsym $mod+r           exec voice-commander start --profile dictate
bindsym $mod+Escape      exec voice-commander cancel
```

## Watch it work

In a spare terminal:

```sh
voice-commander events --follow
```

Every state change appears as one line of JSON. This is also the interface for building your
own indicator — see [events.md](events.md).

## Where things go

```
~/.config/voice-commander/
├── config.toml               your settings
└── conf.d/*.toml             drop-ins, applied in filename order

~/.local/share/voice-commander/
├── recordings/2026/09/11/<session-id>/
│   ├── audio.wav
│   └── session.json          timings, levels, callback results
└── logs/

$XDG_RUNTIME_DIR/voice-commander.sock    the control socket, mode 0600
```

The recordings directory is created mode `0700`. These are recordings of you speaking, and
the default umask is not a good enough reason for anyone else on the machine to read them.

## Uninstall

```sh
systemctl --user disable --now voice-commander
rm ~/.local/bin/voice-commander{,d}
rm ~/.config/systemd/user/voice-commander.service
rm -rf ~/.config/voice-commander      # your settings
rm -rf ~/.local/share/voice-commander # your recordings
```

## Troubleshooting

| Symptom | What to check |
|---|---|
| `no daemon is listening` | `systemctl --user status voice-commander`, or run `voice-commanderd` in a terminal to see why it exits |
| Recordings are silent | `voice-commander mic-test`, then `wpctl status` — the default source is usually muted or is not the one you think |
| The first word is missing | Set `capture.mode = "preroll"` and raise `pre_roll_ms`. The very first recording after the daemon starts has no lookback yet; the second onwards does |
| Recordings are exactly `max_recording_secs` long | The release keybind is not firing. Release the key before the modifier, or switch that profile to `trigger = "toggle"` |
| Two presses became two recordings | You were outside `cooldown_ms`. Raise it |
| `no profile named …` | The error lists the profiles that do exist. Check `voice-commander status` |
| Nothing in the journal | `VOICE_COMMANDER_LOG=debug voice-commanderd` |
| Not sure what to tune | `voice-commander stats` — it reads your own recordings and says |
