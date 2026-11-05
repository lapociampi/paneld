#!/bin/sh
# Numbers about this machine for the system example, read from /proc.
# POSIX sh, awk, grep, df and procps' ps: all on Raspberry Pi OS Lite.
# `top` shows ps's %CPU, the average over each process's lifetime.
#
#   stats.sh cpu      CPU busy %, over one second
#   stats.sh rx|tx    bytes per second received / sent, over one second
#   stats.sh summary  JSON: memory, disk, temperature, load, uptime
#   stats.sh top      JSON: the processes using the most CPU

set -eu

cpu_ticks() {
    # user nice system idle iowait irq softirq steal
    read -r _ u n s i w q sq st _ < /proc/stat
    echo "$((u + n + s + q + sq + st)) $((i + w))"
}

net_bytes() {
    # Every interface but loopback: received (1) or sent (9) bytes.
    awk -v f="$1" -F'[: ]+' 'NR > 2 && $2 != "lo" { s += $(f + 2) } END { print s + 0 }' /proc/net/dev
}

case "${1:-}" in
cpu)
    set -- $(cpu_ticks)
    busy=$1 idle=$2
    sleep 1
    set -- $(cpu_ticks)
    busy=$(($1 - busy)) idle=$(($2 - idle))
    echo $((100 * busy / (busy + idle + 1)))
    ;;
rx | tx)
    field=1
    [ "$1" = tx ] && field=9
    a=$(net_bytes $field)
    sleep 1
    echo $(($(net_bytes $field) - a))
    ;;
summary)
    temp=null
    if [ -r /sys/class/thermal/thermal_zone0/temp ]; then
        temp=$(($(cat /sys/class/thermal/thermal_zone0/temp) / 100))
        temp="$((temp / 10)).$((temp % 10))"
    fi
    read -r l1 l5 l15 _ < /proc/loadavg
    read -r up _ < /proc/uptime
    awk -v temp="$temp" -v l1="$l1" -v l5="$l5" -v l15="$l15" -v up="${up%.*}" \
        -v host="$(cat /proc/sys/kernel/hostname)" -v cores="$(grep -c ^processor /proc/cpuinfo)" '
        /^MemTotal:/ { total = $2 * 1024 }
        /^MemAvailable:/ { avail = $2 * 1024 }
        END {
            "df -Pk / | tail -1" | getline d
            split(d, f, " ")
            printf "{\"host\": \"%s\", \"cores\": %d, \"mem_total\": %d, \"mem_used\": %d, ", host, cores, total, total - avail
            printf "\"disk_total\": %d, \"disk_used\": %d, \"temp\": %s, ", f[2] * 1024, f[3] * 1024, temp
            printf "\"load\": [%s, %s, %s], \"uptime\": %d}\n", l1, l5, l15, up
        }' /proc/meminfo
    ;;
top)
    # The name last: it can hold spaces.
    ps -eo pcpu=,rss=,comm= --sort=-pcpu | awk '
        BEGIN { printf "[" }
        $3 != "ps" && n < 6 {
            n++; cpu = $1; mem = $2 * 1024
            sub(/^ *[^ ]+ +[^ ]+ +/, ""); gsub(/["\\]/, "")
            printf "%s{\"name\": \"%s\", \"cpu\": %s, \"mem\": %d}", (n > 1 ? ", " : ""), $0, cpu, mem
        }
        END { print "]" }'
    ;;
*)
    echo "usage: stats.sh cpu|rx|tx|summary|top" >&2
    exit 2
    ;;
esac
