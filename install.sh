#!/usr/bin/env bash
#
# voice-commander installer.
#
# Builds the two binaries, puts them somewhere on your PATH, installs the systemd user
# service, and checks that your microphone actually works — because a silent or muted input
# is the single most common reason this appears not to work at all.
#
#   ./install.sh                 install to ~/.local, ask before each optional step
#   ./install.sh --yes           accept every default, no questions
#   ./install.sh --uninstall     remove what was installed (never your recordings)
#   ./install.sh --help          everything else

set -euo pipefail

# Resolved with bash's own parameter expansion rather than `dirname`: an assignment to a
# `readonly` does not trip `set -e` when its command substitution fails, so a missing
# external tool here would silently leave REPO empty and fail much later, somewhere
# confusing.
script_dir="${BASH_SOURCE[0]%/*}"
[ "$script_dir" = "${BASH_SOURCE[0]}" ] && script_dir="."
REPO="$(cd -- "$script_dir" && pwd)" || {
    printf 'error: cannot determine where this script lives\n' >&2
    exit 1
}
readonly REPO

# Someone may well have downloaded just this file. Say so plainly rather than letting cargo
# fail in a way that reads like a build problem.
if [ ! -f "${REPO}/Cargo.toml" ] || [ ! -d "${REPO}/crates" ]; then
    printf 'error: run this from inside a voice-commander checkout\n\n' >&2
    printf '  git clone https://github.com/apep-1998/voice-commander\n' >&2
    printf '  cd voice-commander\n' >&2
    printf '  ./install.sh\n' >&2
    exit 1
fi

PREFIX="${HOME}/.local"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/voice-commander"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/voice-commander"

readonly PATH_MARKER_BEGIN="# >>> voice-commander >>>"
readonly PATH_MARKER_END="# <<< voice-commander <<<"

ASSUME_YES=0
DO_SERVICE=1
DO_CONFIG=1
DO_MICTEST=1
DO_PATH=1
UNINSTALL=0

# ── output ───────────────────────────────────────────────────────────────────

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'
    YELLOW=$'\033[33m'; BLUE=$'\033[34m'; RESET=$'\033[0m'
else
    BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; BLUE=''; RESET=''
fi

step()  { printf '\n%s==>%s %s%s%s\n' "$BLUE" "$RESET" "$BOLD" "$*" "$RESET"; }
info()  { printf '    %s\n' "$*"; }
ok()    { printf '    %s✓%s %s\n' "$GREEN" "$RESET" "$*"; }
warn()  { printf '    %s!%s %s\n' "$YELLOW" "$RESET" "$*"; }
die()   { printf '\n%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

# Ask a yes/no question. Defaults to yes; --yes and a non-interactive shell both skip it.
confirm() {
    [ "$ASSUME_YES" = 1 ] && return 0
    [ -t 0 ] || return 0
    local reply
    printf '    %s [Y/n] ' "$1"
    read -r reply || return 0
    case "$reply" in
        [nN]*) return 1 ;;
        *)     return 0 ;;
    esac
}

usage() {
    cat <<EOF
${BOLD}voice-commander installer${RESET}

  ./install.sh [options]

${BOLD}Options${RESET}
  --prefix DIR     install binaries to DIR/bin (default: ~/.local)
  --yes, -y        accept every default without asking
  --no-service     do not install or enable the systemd user service
  --no-config      do not write a starter configuration
  --no-mic-test    do not check the microphone afterwards
  --no-path        do not offer to put the install directory on your PATH
  --uninstall      remove the binaries and the service
  --help, -h       this

${BOLD}What gets installed${RESET}
  PREFIX/bin/voice-commander         the client your keybind runs
  PREFIX/bin/voice-commanderd        the daemon
  PREFIX/bin/voice-commander-overlay the on-screen indicator
  ${UNIT_DIR#$HOME/}/voice-commander.service
  ${CONFIG_DIR#$HOME/}/config.toml   (only if you do not already have one)

Your recordings and your configuration are never removed by --uninstall.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)      PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --prefix=*)    PREFIX="${1#*=}"; shift ;;
        -y|--yes)      ASSUME_YES=1; shift ;;
        --no-service)  DO_SERVICE=0; shift ;;
        --no-config)   DO_CONFIG=0; shift ;;
        --no-mic-test) DO_MICTEST=0; shift ;;
        --no-path)     DO_PATH=0; shift ;;
        --uninstall)   UNINSTALL=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        *)             die "unknown option: $1 (try --help)" ;;
    esac
done

readonly BIN_DIR="${PREFIX}/bin"

# ── PATH ─────────────────────────────────────────────────────────────────────

# The file this user's shell reads on startup, and how it spells a PATH addition.
#
# Set by `shell_profile` into PROFILE_FILE and PROFILE_LINE, because returning two values
# from a shell function is otherwise more trouble than it is worth.
PROFILE_FILE=""
PROFILE_LINE=""

shell_profile() {
    local shell_name
    shell_name="$(basename -- "${SHELL:-}" 2>/dev/null || true)"
    # $SHELL is not always set — cron, some desktop launchers — so fall back to the account.
    if [ -z "$shell_name" ] && command -v getent >/dev/null 2>&1; then
        shell_name="$(basename -- "$(getent passwd "$(id -un)" | cut -d: -f7)")"
    fi

    case "$shell_name" in
        zsh)
            PROFILE_FILE="${ZDOTDIR:-$HOME}/.zshrc"
            PROFILE_LINE="export PATH=\"${BIN_DIR}:\$PATH\""
            ;;
        bash)
            # .bashrc on Linux: that is what an interactive shell reads, and it is where
            # people already keep their PATH additions.
            if [ -f "${HOME}/.bashrc" ]; then
                PROFILE_FILE="${HOME}/.bashrc"
            else
                PROFILE_FILE="${HOME}/.bash_profile"
            fi
            PROFILE_LINE="export PATH=\"${BIN_DIR}:\$PATH\""
            ;;
        fish)
            PROFILE_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
            # fish has its own idempotent helper, so this is safe to run more than once.
            PROFILE_LINE="fish_add_path ${BIN_DIR}"
            ;;
        *)
            PROFILE_FILE="${HOME}/.profile"
            PROFILE_LINE="export PATH=\"${BIN_DIR}:\$PATH\""
            ;;
    esac
}

# Whether the profile already mentions this directory.
#
# Matches the directory rather than a particular line, so a line the user wrote themselves is
# recognised and not duplicated — and it has to match the *spellings people actually use*.
# `export PATH="$HOME/.local/bin:$PATH"` never contains the expanded path, so checking only
# for that would add a second line to a profile that was already correct.
profile_mentions_bin_dir() {
    [ -f "$PROFILE_FILE" ] || return 1

    local candidate
    for candidate in $(bin_dir_spellings); do
        grep -qF -- "$candidate" "$PROFILE_FILE" 2>/dev/null && return 0
    done
    return 1
}

# The ways this directory might be written in a shell profile.
bin_dir_spellings() {
    printf '%s\n' "$BIN_DIR"
    case "$BIN_DIR" in
        "$HOME"/*)
            local relative="${BIN_DIR#"$HOME"/}"
            printf '%s\n' "\$HOME/${relative}"
            printf '%s\n' "\${HOME}/${relative}"
            printf '%s\n' "~/${relative}"
            ;;
    esac
}

ensure_on_path() {
    case ":${PATH}:" in
        *":${BIN_DIR}:"*)
            ok "${BIN_DIR} is on your PATH"
            return 0
            ;;
    esac

    shell_profile

    if profile_mentions_bin_dir; then
        # Already written, just not in *this* shell — the usual case when the profile was
        # edited a moment ago, or by an earlier run of this script.
        ok "${PROFILE_FILE} already puts ${BIN_DIR} on your PATH"
        info "open a new terminal, or run: ${BOLD}exec \$SHELL${RESET}"
        return 0
    fi

    warn "${BIN_DIR} is not on your PATH, so the command will not be found"

    if [ "$DO_PATH" = 0 ]; then
        info "add this to ${PROFILE_FILE} yourself:"
        printf '      %s%s%s\n' "$BOLD" "$PROFILE_LINE" "$RESET"
        return 0
    fi

    if ! confirm "Add it to ${PROFILE_FILE/#$HOME/\~}?"; then
        info "not added. To do it later:"
        printf '      %secho %s >> %s%s\n' "$BOLD" "'$PROFILE_LINE'" "$PROFILE_FILE" "$RESET"
        return 0
    fi

    mkdir -p -- "$(dirname -- "$PROFILE_FILE")"
    # Marked so --uninstall can find exactly what was added and nothing else.
    {
        printf '\n%s\n' "$PATH_MARKER_BEGIN"
        printf '%s\n' "$PROFILE_LINE"
        printf '%s\n' "$PATH_MARKER_END"
    } >> "$PROFILE_FILE"

    ok "added to ${PROFILE_FILE}"
    info "it applies to new shells — for this one, run: ${BOLD}exec \$SHELL${RESET}"
}

# Removes only the block this script wrote, matched by its markers. A user's own PATH line
# is never touched.
remove_path_entry() {
    shell_profile
    [ -f "$PROFILE_FILE" ] || return 0
    grep -qF -- "$PATH_MARKER_BEGIN" "$PROFILE_FILE" 2>/dev/null || return 0

    local temp
    temp="$(mktemp)" || return 0
    awk -v begin="$PATH_MARKER_BEGIN" -v end="$PATH_MARKER_END" '
        $0 == begin { skipping = 1; next }
        $0 == end   { skipping = 0; next }
        !skipping   { print }
    ' "$PROFILE_FILE" > "$temp"

    # Only replace the file if the result still parses as something sane — a truncated
    # shell profile is a genuinely bad thing to leave behind.
    if [ -s "$temp" ]; then
        cat "$temp" > "$PROFILE_FILE"
        rm -f "$temp"
        ok "removed the PATH entry from ${PROFILE_FILE}"
    else
        rm -f "$temp"
        warn "left ${PROFILE_FILE} alone — removing the entry would have emptied it"
    fi
}

# ── uninstall ────────────────────────────────────────────────────────────────

if [ "$UNINSTALL" = 1 ]; then
    step "Removing voice-commander"

    if command -v systemctl >/dev/null 2>&1; then
        if systemctl --user list-unit-files voice-commander.service >/dev/null 2>&1; then
            systemctl --user disable --now voice-commander.service >/dev/null 2>&1 || true
            ok "stopped and disabled the service"
        fi
    fi
    rm -f "${UNIT_DIR}/voice-commander.service"
    command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload >/dev/null 2>&1 || true

    for binary in voice-commander voice-commanderd voice-commander-overlay; do
        if [ -e "${BIN_DIR}/${binary}" ]; then
            rm -f "${BIN_DIR}/${binary}"
            ok "removed ${BIN_DIR}/${binary}"
        fi
    done

    remove_path_entry

    # Deliberately kept. Removing someone's recordings or their configuration because they
    # uninstalled a binary would be a nasty surprise; the paths are printed instead.
    printf '\n'
    info "left alone:"
    info "  ${CONFIG_DIR}  (your configuration)"
    info "  ${DATA_DIR}  (your recordings)"
    info "remove them yourself if you want them gone."
    exit 0
fi

# ── prerequisites ────────────────────────────────────────────────────────────

# The package name for a missing build dependency, per distro.
suggest_packages() {
    if   command -v pacman  >/dev/null 2>&1; then echo "sudo pacman -S --needed rust alsa-lib pkgconf"
    elif command -v apt-get >/dev/null 2>&1; then echo "sudo apt install cargo libasound2-dev pkg-config"
    elif command -v dnf     >/dev/null 2>&1; then echo "sudo dnf install cargo alsa-lib-devel pkgconf-pkg-config"
    elif command -v zypper  >/dev/null 2>&1; then echo "sudo zypper install cargo alsa-devel pkg-config"
    else echo "install: a Rust toolchain, the ALSA development headers, and pkg-config"
    fi
}

step "Checking prerequisites"

missing=0
if command -v cargo >/dev/null 2>&1; then
    ok "cargo $(cargo --version | awk '{print $2}')"
else
    warn "cargo is not installed"
    missing=1
fi

# cpal links against ALSA even on a PipeWire system — it reaches the sound server through
# ALSA's compatibility layer — so the headers are needed to build at all.
if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists alsa; then
    ok "ALSA development headers"
else
    warn "the ALSA development headers are missing (needed to build the audio backend)"
    missing=1
fi

if [ "$missing" = 1 ]; then
    printf '\n'
    info "install them with:"
    printf '      %s%s%s\n' "$BOLD" "$(suggest_packages)" "$RESET"
    die "missing build dependencies"
fi

# Not required to build, only to run. Worth saying now rather than at the first failed
# callback.
if command -v pw-cli >/dev/null 2>&1 || command -v pactl >/dev/null 2>&1; then
    ok "a sound server is running"
else
    warn "no PipeWire or PulseAudio found — recording will not work until one is running"
fi

for optional in "wl-copy:the clipboard callback" \
                "wtype:typing the transcript into the focused window" \
                "notify-send:desktop notifications"; do
    program="${optional%%:*}"
    purpose="${optional#*:}"
    command -v "$program" >/dev/null 2>&1 \
        || info "${DIM}optional: ${program} is not installed — needed for ${purpose}${RESET}"
done

# ── build ────────────────────────────────────────────────────────────────────

step "Building (this takes a couple of minutes the first time)"
cd "$REPO"
if ! cargo build --release --workspace; then
    die "the build failed — see the output above"
fi
ok "built voice-commander and voice-commanderd"

# ── install ──────────────────────────────────────────────────────────────────

step "Installing to ${BIN_DIR}"
mkdir -p "$BIN_DIR"
for binary in voice-commander voice-commanderd voice-commander-overlay; do
    install -Dm755 "${REPO}/target/release/${binary}" "${BIN_DIR}/${binary}"
    ok "${BIN_DIR}/${binary}"
done

ensure_on_path

# ── configuration ────────────────────────────────────────────────────────────

if [ "$DO_CONFIG" = 1 ]; then
    step "Configuration"
    if [ -f "${CONFIG_DIR}/config.toml" ]; then
        # Never overwrite: this is the user's own file, and they may have an API key
        # command or a set of callbacks in it.
        ok "${CONFIG_DIR}/config.toml already exists — leaving it alone"
    elif confirm "Write a commented starter configuration to ${CONFIG_DIR}/config.toml?"; then
        mkdir -p "$CONFIG_DIR"
        "${BIN_DIR}/voice-commander" config init --path "${CONFIG_DIR}/config.toml" >/dev/null
        ok "wrote ${CONFIG_DIR}/config.toml"
        info "everything in it is optional — delete what you do not want to change"
    else
        info "skipped; the built-in defaults record and archive with no configuration at all"
    fi

    if [ -f "${CONFIG_DIR}/config.toml" ]; then
        if "${BIN_DIR}/voice-commander" config check --config-dir "$CONFIG_DIR" >/dev/null 2>&1; then
            ok "configuration is valid"
        else
            warn "configuration has problems:"
            "${BIN_DIR}/voice-commander" config check --config-dir "$CONFIG_DIR" 2>&1 | sed 's/^/      /'
        fi
    fi
fi

# ── service ──────────────────────────────────────────────────────────────────

if [ "$DO_SERVICE" = 1 ]; then
    step "Systemd user service"
    if ! command -v systemctl >/dev/null 2>&1; then
        warn "systemd not found — start the daemon yourself with: voice-commanderd"
    else
        mkdir -p "$UNIT_DIR"
        # The shipped unit points at ~/.local/bin; rewrite it if installing elsewhere.
        sed "s|%h/.local/bin/voice-commanderd|${BIN_DIR}/voice-commanderd|" \
            "${REPO}/packaging/voice-commander.service" > "${UNIT_DIR}/voice-commander.service"
        ok "installed ${UNIT_DIR}/voice-commander.service"
        systemctl --user daemon-reload

        if confirm "Start it now and on every login?"; then
            if systemctl --user enable --now voice-commander.service 2>/dev/null; then
                sleep 1
                if systemctl --user is-active --quiet voice-commander.service; then
                    ok "running"
                else
                    warn "it did not stay running — see: journalctl --user -u voice-commander -n 30"
                fi
            else
                # A user service needs a session bus, which an ssh session without lingering
                # does not have. Saying so beats a bare failure.
                warn "could not enable it — if you are over ssh, try: loginctl enable-linger $USER"
            fi
        else
            info "start it later with: systemctl --user enable --now voice-commander"
        fi
    fi
fi

# ── microphone ───────────────────────────────────────────────────────────────

if [ "$DO_MICTEST" = 1 ] && [ -t 0 ]; then
    step "Checking your microphone"
    info "a muted or wrong input is the most common reason this appears not to work"
    if confirm "Record two seconds now?"; then
        "${BIN_DIR}/voice-commander" mic-test --seconds 2 2>&1 \
            | grep -vE '^ALSA lib' | sed 's/^/      /' || true
    fi
fi

# ── next steps ───────────────────────────────────────────────────────────────

step "Done"
cat <<EOF
    Try it:

      ${BOLD}voice-commander-overlay --demo${RESET}         see the overlay, no microphone needed
      ${BOLD}voice-commander mic-test${RESET}              check the microphone
      ${BOLD}voice-commander status${RESET}                what the daemon is doing
      ${BOLD}voice-commander events --follow${RESET}       watch it work, in another terminal

      ${BOLD}voice-commander start --profile default${RESET}   …say something…
      ${BOLD}voice-commander stop  --profile default${RESET}

    Then bind it, in ~/.config/hypr/hyprland.conf:

      bind  = SUPER, M, exec, voice-commander start --profile default
      bindr = SUPER, M, exec, voice-commander stop  --profile default
      bind  = SUPER, Escape, exec, voice-commander cancel

    Configurations you can copy — local dictation, cloud transcription,
    talking to an agent, a voice journal:

      ${DIM}${REPO}/docs/recipes.md${RESET}
EOF
