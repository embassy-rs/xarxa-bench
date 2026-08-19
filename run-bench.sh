#!/bin/bash
# Build one benchmark, flash it onto the Nucleo on the bcnma device farm, let it run,
# and print both the board's output and what perf-server saw.
#
#   ./run-bench.sh stack-xarxa bench-tcp-rx [ipv4|ipv6] [seconds]
#
# The IP version defaults to ipv4.
#
# The board keeps running after the debugger detaches, so a TX benchmark keeps
# saturating the link until the next flash. `./run-bench.sh stop` erases it.

set -euo pipefail

HOST=root@bcnma.akiles.xyz
PORT=7022
PROBE=0483:374b:0670FF495254707867252236
CHIP=STM32F429ZITx
PROBE_RS=/root/.cargo/bin/probe-rs
ELF=target/thumbv7em-none-eabihf/release/xarxa-example-stm32f429zi

if [ "${1:-}" = "stop" ]; then
    ssh -p $PORT $HOST "pkill -x probe-rs || true; sleep 2; $PROBE_RS erase --chip $CHIP --probe $PROBE"
    exit 0
fi

USAGE="usage: run-bench.sh <stack-xarxa|stack-smoltcp> <bench-...> [ipv4|ipv6] [seconds] | stop"
STACK=${1:?$USAGE}
BENCH=${2:?$USAGE}
IPV=${3:-ipv4}
SECS=${4:-20}

cargo build --release --features "$STACK,$BENCH,$IPV"
scp -P $PORT "$ELF" $HOST:/root/xarxa-f429 >/dev/null

ssh -p $PORT $HOST "
    pkill -x probe-rs || true
    sleep 2
    rm -f /root/xarxa.log
    (setsid $PROBE_RS run --chip $CHIP --probe $PROBE /root/xarxa-f429 \
        > /root/xarxa.log 2>&1 < /dev/null &)
    sleep $SECS
    pkill -x probe-rs || true
    sleep 1
    echo '=== board ==='
    sed 's/\x1b\[[0-9;]*m//g' /root/xarxa.log | grep -v '^ *Frame \|^ */home\|^Core \|stack backtrace\|Received SIGTERM\|Exited by user'
    echo '=== perf-server ==='
    journalctl -u perf-server --since '-${SECS}s' --no-pager -o cat | tail -20
"
