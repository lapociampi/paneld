#!/bin/sh
# Demo stream statistics that move like a live stream's: viewers drift,
# the bitrate wobbles, and every few minutes frames drop for a while.
# `stats.sh bitrate` prints only the bitrate (kb/s), for its history.
t=$(date +%s)
awk -v t="$t" -v only="$1" 'BEGIN {
    srand(t)
    viewers = 1180 + int(140 * sin(t / 40)) + int(rand() * 25)
    bitrate = 6000 + int(450 * sin(t / 7)) + int(rand() * 300)
    dropped = (t % 180 < 20) ? 2.4 + rand() : rand() * 0.2
    fps     = (t % 180 < 20) ? 57 : 60
    cpu     = 18 + int(6 * sin(t / 11)) + int(rand() * 4)
    if (only == "bitrate") { print bitrate; exit }
    printf "{\"viewers\": %d, \"bitrate\": %d, \"dropped\": %.1f, \"fps\": %d, \"cpu\": %d}\n", viewers, bitrate, dropped, fps, cpu
}'
