#!/bin/sh
# Demo only: an hour of believable round-trip times (one every 30s),
# ending now, so the chart moves like a real one. A real rack keeps
# `history = 1h` on a ping source instead (see paneld.conf).
awk -v t="$(date +%s)" 'BEGIN {
    printf "["
    for (i = 119; i >= 0; i--) {
        s = int((t - i * 30) / 30)
        srand(s); v = 11 + rand() * 4 + 3 * sin(s / 9)
        if (rand() < 0.06) v += 20 + rand() * 15
        printf "%s%.1f", (i < 119 ? ", " : ""), v
    }
    print "]"
}'
