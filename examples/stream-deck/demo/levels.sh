#!/bin/sh
# Demo audio levels (0-100) for the mixer's meters: speech comes in
# bursts, the game and the music hold steadier.
t=$(date +%s)
awk -v t="$t" 'BEGIN {
    srand(t)
    mic     = (t % 7 < 4) ? 45 + int(rand() * 40) : int(rand() * 8)
    desktop = 50 + int(20 * sin(t / 3)) + int(rand() * 10)
    music   = 35 + int(10 * sin(t / 5)) + int(rand() * 8)
    chat    = (t % 11 < 3) ? 30 + int(rand() * 30) : 0
    printf "{\"mic\": %d, \"desktop\": %d, \"music\": %d, \"chat\": %d}\n", mic, desktop, music, chat
}'
