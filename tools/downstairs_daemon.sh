#!/bin/bash
#
# A hack of downstairs restart tool
#
# Start up three downstairs in a retry loop.  If they are killed then start
# them up again.
# Start another loop to kill a downstairs at some random time.
# Let the restarts begin.
# When we have had enough, then touch the stop file and make everything
# wind down.
#
# No error checking really, we assume you have a proper directory for
# each downstairs.  Too much hard coded here.

set -o pipefail

# Control-C to cleanup.
trap ctrl_c INT
function ctrl_c() {
    echo "Stopping at your request"
    rm -f "$testdir"/up
    rm -f "$testdir"/pause
    touch "$testdir"/stop
}

# This loop will sleep some random time, then kill a downstairs.
# We currently pick
downstairs_restart() {
    while :; do
        if [[ -f ${testdir}/up ]]; then
            sleep 5
            continue
        fi
        if [[ -f ${testdir}/pause ]]; then
            ds_pids=$(pgrep -fl -U $(id -u) ${cds} | awk '{print $1}')
            for pid in ${ds_pids}; do
                echo "pause downstairs, stop PID $pid"
                kill "$pid"
            done
            sleep 5
            continue
        fi

        # How long we sleep before restarting the next PID
        sleeptime=$(echo "$(date +%s) % 5" | bc )
        sleep "$sleeptime"
        if [[ -f ${testdir}/stop ]]; then
            break
        fi

        # Pick a PID and kill it
        ds_pids=( $(pgrep -fl -U $(id -u) ${cds} | awk '{print $1}') )

        # Sometimes there are no downstairs running.
        if [[ ${#ds_pids[@]} -gt 0 ]]; then
            pid_index=$((RANDOM % ${#ds_pids[@]}))

            #echo "Kill ${ds_pids[$pid_index]}"
            kill "${ds_pids[$pid_index]}"
            # > /dev/null 2>&1
        fi
    done
    # Run a final cleanup
    ds=$(pgrep -fl -U $(id -u) ${cds} | awk '{print $1}')
    for pid in ${ds}; do
        kill "$pid"
    done
    echo "exit downstairs restarter"
}

# Loop restarting a downstairs at the given port.
# If we get stopped for any reason other than 143, then report error
# and stop looping.
downstairs_daemon() {
    port=$1
    outfile="${testdir}/downstairs-out-${port}.txt"
    errfile="${testdir}/downstairs-err-${port}.txt"
    echo "" > "$outfile"
    echo "" > "$errfile"
    echo "$(date) Starting downstairs ${port}"
    while :; do
        if [[ -f ${testdir}/pause ]]; then
            sleep 3
            continue
        fi

        ${cds} run -p "$port" -d var/"$port">> "$outfile" 2> "$errfile"
        res=$?
        if [[ $res -ne 143 ]]; then
            echo "Downstairs $port exited with: $res"
            exit $res
        fi
        echo "$(date) Downstairs ${port} ended"

        sleep 1
        if [[ -f ${testdir}/stop ]]; then
            break
        fi
        echo "$(date) Restaring downstairs ${port}"

    done
    echo "$(date) downstairs ${port} exit on request"
}

ROOT=$(cd "$(dirname "$0")/.." && pwd)

cd "$ROOT" || (echo failed to cd "$ROOT"; exit 1)

run_on_start=0
release_build=0
while getopts 'ru' opt; do
    case "$opt" in
        u)  run_on_start=1
            echo "Run on start"
            ;;
        r)  release_build=1
            export BINDIR=${ROOT}/target/release
            echo "Using $BINDIR for binaries"
            ;;
        *)  echo "Usage: $0 [-u]" >&2
            echo "u: Don't restart downstairs initially"
            echo "r: Use release build for downstairs"
            exit 1
            ;;
    esac
done

# Remove all options passed by getopts options
shift $((OPTIND-1))

export BINDIR=${BINDIR:-$ROOT/debug}
cds="$BINDIR/crucible-downstairs"
dsc="$BINDIR/dsc"

if pgrep -fl -U $(id -u) "$cds"; then
    echo 'Some downstairs already running?' >&2
    exit 1
fi

if [[ ! -f ${cds} ]]; then
    echo "Can't find crucible binary at $cds"
    exit 1
fi

os_name=$(uname)
if [[ "$os_name" == 'Darwin' ]]; then
    # stupid macos needs this to avoid popup hell.
    codesign -s - -f "$cds"
fi

# If this port base is different than default, then good luck..
port_base=8810
missing=0
for (( i = 0; i < 3; i++ )); do
    (( port_step = i * 10 ))
    (( port = port_base + port_step ))
    if [[ ! -d var/${port} ]]; then
        echo "Missing var/${port} directory"
        missing=1
    fi
done
if [[ missing -eq 1 ]]; then
    if ! "$dsc" create --region-dir ./var \
            --block-size 512 --extent-size 100 --extent-count 20 ; then
        echo "Failed to create region directories"
        exit 1
    fi
    echo "Created NEW test region directories"
else
    echo "Using existing region directories"
fi

echo "Using $cds for crucible downstairs"

testdir="/var/tmp/ds_test"
if [[ -d ${testdir} ]]; then
    rm -rf ${testdir}
fi

mkdir -p ${testdir}
for (( i = 0; i < 3; i++ )); do
    (( port_step = i * 10 ))
    (( port = port_base + port_step ))
    downstairs_daemon "$port" 2>/dev/null &
    dsd_pid["$i"]=$!
done

echo "Downstairs have been started"

if [[ $run_on_start -eq 1 ]]; then
    echo Downstairs will remain up until "$testdir"/up is removed
    touch ${testdir}/up
fi
sleep 1

downstairs_restart &
dsd_pid[3]=$!

# Loop our known background jobs, if any disappear, then stop
# the script.
while :; do
    for pid in ${dsd_pid[*]}; do
        if ! ps -p $pid > /dev/null; then
            if [[ -f ${testdir}/stop ]]; then
                rm -f ${testdir}/up
                echo "Stop requested for $pid"
            else
                echo "Downstairs PID: $pid is gone, check $testdir for errors"
                rm -f ${testdir}/up
                touch ${testdir}/stop
            fi
        fi
    done
    if [[ -f ${testdir}/stop ]]; then
        echo "Stopping loop"
        rm -f ${testdir}/up
        break
    fi
    sleep 10
done

# Cleanup leftovers
ds=$(pgrep -fl -U $(id -u) ${cds} | awk '{print $1}')
for pid in ${ds}; do
    kill "$pid"
done

echo "Downstairs will all now stop for good"
rm -f ${testdir}/up
rm -f ${testdir}/pause
for pid in ${dsd_pid[*]}; do
    kill "$pid"
    wait "$pid"
done

rm -f ${testdir}/stop
