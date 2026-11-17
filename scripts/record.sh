#!/bin/sh
# Record one Jellyfin invocation as a JSON line, then run it unchanged.
#
#   RRFFMPEG_RECORD=/var/log/rrffmpeg/jellyfin.jsonl \
#   RRFFMPEG_RECORD_LABEL=ffmpeg \
#     scripts/record.sh /usr/lib/jellyfin-ffmpeg/ffmpeg -i in.mkv out.m3u8
#
# The command replaces this script with `exec`, so stdio, signals and the exit
# code stay exactly as they were. A failed write warns and lets the job run:
# recording must never break playback.
set -u

if [ "$#" -eq 0 ]; then
    echo "usage: record.sh <command> [args...]" >&2
    exit 2
fi

json_string() {
    # Escape what JSON needs from an ordinary argument. Newlines and tabs are
    # left alone: no ffmpeg argument carries them.
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

record() {
    program=${1##*/}
    shift
    line='{"program":"'
    line="$line$(json_string "$program")\",\"argv\":["
    sep=''
    for arg in "$@"; do
        line="$line$sep\"$(json_string "$arg")\""
        sep=','
    done
    line="$line],\"cwd\":\"$(json_string "$PWD")\""
    if [ -n "${RRFFMPEG_RECORD_LABEL:-}" ]; then
        line="$line,\"label\":\"$(json_string "$RRFFMPEG_RECORD_LABEL")\""
    fi
    line="$line}"
    # One short line on an O_APPEND descriptor: concurrent dispatchers append
    # whole lines instead of interleaving.
    printf '%s\n' "$line" >> "$RRFFMPEG_RECORD"
}

if [ -n "${RRFFMPEG_RECORD:-}" ]; then
    record "$@" || echo "record.sh: cannot append to $RRFFMPEG_RECORD" >&2
fi

exec "$@"
