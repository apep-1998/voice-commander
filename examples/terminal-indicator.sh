#!/usr/bin/env sh
#
# A live indicator for voice-commander, in a terminal.
#
#   ./terminal-indicator.sh
#
# Shows a level meter while you speak and a per-callback progress list while the pipeline
# runs. It exists to be *read*: this is the whole of what a graphical overlay would have to
# do, and it consumes exactly the same event stream one would — no privileged access, no
# extra API, about a hundred lines of shell.
#
# Needs: jq.

set -eu

command -v jq >/dev/null 2>&1 || { echo "this needs jq" >&2; exit 1; }
command -v voice-commander >/dev/null 2>&1 || { echo "voice-commander is not on PATH" >&2; exit 1; }

ESC=$(printf '\033')
CLEAR="${ESC}[2K\r"
DIM="${ESC}[2m"; RED="${ESC}[31m"; GREEN="${ESC}[32m"; YELLOW="${ESC}[33m"; RESET="${ESC}[0m"

# A level bar. dBFS is negative: about -60 is silence, -12 is a good speaking level.
meter() {
    filled=$(awk -v d="$1" 'BEGIN { v = int((d + 60) / 3); if (v < 0) v = 0; if (v > 20) v = 20; print v }')
    if   [ "$3" = "true" ]; then colour=$RED      # clipping
    elif [ "$2" = "true" ]; then colour=$GREEN    # speech, not just room tone
    else colour=$DIM
    fi
    bar=""; i=0
    while [ "$i" -lt 20 ]; do
        if [ "$i" -lt "$filled" ]; then bar="${bar}#"; else bar="${bar}."; fi
        i=$((i + 1))
    done
    printf '%s%s%s' "$colour" "$bar" "$RESET"
}

printf '%swaiting for voice-commander...%s\n' "$DIM" "$RESET"

voice-commander events --follow | while IFS= read -r line; do
    event=$(printf '%s' "$line" | jq -r '.event // empty')
    [ -n "$event" ] || continue

    case "$event" in
    recording_started)
        printf '%s%s* listening%s  (%sms of lookback)\n' "$CLEAR" "$RED" "$RESET" \
            "$(printf '%s' "$line" | jq -r '.pre_roll_ms')"
        ;;

    level)
        # The one event worth drawing continuously. It is what tells the user their voice is
        # arriving, rather than merely that the microphone is open.
        rms=$(printf '%s' "$line" | jq -r '.rms_dbfs')
        printf '%s  %s %6.1f dBFS' "$CLEAR" \
            "$(meter "$rms" "$(printf '%s' "$line" | jq -r '.speech')" \
                     "$(printf '%s' "$line" | jq -r '.clipping')")" "$rms"
        ;;

    input_warning)
        case "$(printf '%s' "$line" | jq -r '.kind')" in
            device_muted|no_device|silence)
                printf '%s  %s! %s%s\n' "$CLEAR" "$YELLOW" \
                    "$(printf '%s' "$line" | jq -r '.kind')" "$RESET" ;;
        esac
        ;;

    recording_stopped)
        printf '%s  stopped after %sms (%s)\n' "$CLEAR" \
            "$(printf '%s' "$line" | jq -r '.duration_ms')" \
            "$(printf '%s' "$line" | jq -r '.reason')"
        ;;

    cooldown_started)
        # Not the end of the session: pressing again inside this window continues it. An
        # indicator that hides here flickers every time the user pauses to think.
        printf '  %s... %sms to say more%s\n' "$DIM" \
            "$(printf '%s' "$line" | jq -r '.ms')" "$RESET"
        ;;

    recording_resumed)
        printf '%s%s* still listening%s  (continued after %sms)\n' "$CLEAR" "$RED" "$RESET" \
            "$(printf '%s' "$line" | jq -r '.resumed_after_ms')"
        ;;

    session_finalized)
        printf '  saved %ss of audio\n' \
            "$(printf '%s' "$line" | jq -r '.total_ms' | awk '{ printf "%.1f", $1 / 1000 }')"
        ;;

    pipeline_started)
        # The plan arrives complete, before anything runs — which is exactly what lets a
        # progress list be drawn in full and greyed out, rather than growing as it goes.
        printf '%s' "$line" | jq -r --arg dim "$DIM" --arg reset "$RESET" '
            (if .transcriber then "  transcribing with \(.transcriber)..." else empty end),
            (.sinks[] | "  \($dim)o \(.name) (\(.kind))\($reset)")'
        ;;

    transcribe_done)
        printf '  %s+%s transcribed %s chars in %sms\n' "$GREEN" "$RESET" \
            "$(printf '%s' "$line" | jq -r '.chars')" \
            "$(printf '%s' "$line" | jq -r '.latency_ms')"
        ;;

    transcribe_failed)
        printf '  %sx%s transcription failed: %s\n' "$RED" "$RESET" \
            "$(printf '%s' "$line" | jq -r '.error')"
        ;;

    sink_finished)
        # Three outcomes, three renderings. A greyed row always says why.
        printf '%s' "$line" | jq -r \
            --arg ok "$GREEN" --arg bad "$RED" --arg dim "$DIM" --arg reset "$RESET" '
            if   .outcome.status == "ok"     then "  \($ok)+\($reset) \(.name)  \(.latency_ms)ms"
            elif .outcome.status == "failed" then "  \($bad)x\($reset) \(.name)  \(.outcome.error)"
            else "  \($dim)- \(.name)  skipped: \(.outcome.reason)\($reset)" end'
        ;;

    pipeline_finished)
        printf '%s' "$line" | jq -r '"  \(.ok) done, \(.failed) failed, \(.skipped) skipped  (\(.total_ms)ms)"'
        printf '\n'
        ;;

    error)
        printf '  %sx %s: %s%s\n' "$RED" \
            "$(printf '%s' "$line" | jq -r '.stage')" \
            "$(printf '%s' "$line" | jq -r '.message')" "$RESET"
        ;;
    esac
done
