#!/bin/bash

# A test to break, then Repair a downstairs region that is out of sync with
# the other regions. We pick a downstairs at random and restart it with
# the --lossy flag, meaning it will skip some IO requests (and have to
# come back and do them later) and will introduce some delay in completing
# IOs.  This combined with the client program exiting as soon as an IO
# is acked means that the lossy downstairs will always be missing IOs.

trap ctrl_c INT
function ctrl_c() {
    echo "Stopping at your request"
    cleanup
    exit 1
}

function cleanup() {
    kill "$ds0_pid" 2> /dev/null || true
    kill "$ds1_pid" 2> /dev/null || true
    kill "$ds2_pid" 2> /dev/null || true
}

set -o errexit
set -o pipefail

SECONDS=0
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export BINDIR=${BINDIR:-$ROOT/target/debug}

cds="$BINDIR/crucible-downstairs"
ct="$BINDIR/crutest"
dsc="$BINDIR/dsc"

for bin in $cds $ct $dsc; do
    if [[ ! -f "$bin" ]]; then
        echo "Can't find crucible binary at $bin" >&2
        exit 1
    fi
done

# For buildomat, the regions should be in /var/tmp
testdir="/var/tmp/test_repair"
if [[ -d ${testdir} ]]; then
    rm -rf ${testdir}
fi

verify_file=/tmp/test_repair_verify.data
test_log=/tmp/test_repair_out.txt
ds_log_prefix=/tmp/test_repair_ds

dump_args=()
while getopts 'N' opt; do
	case "$opt" in
		N)  echo "Turn off color for downstairs dump"
            dump_args+=(" --no-color")
            ;;
        *)
			echo "Usage: $0 [N]"
			echo "N:  Don't dump color output"
			exit 1
			;;
	esac
done

if ! "$dsc" create --cleanup --ds-bin "$cds" --extent-count 30 --extent-size 20 --region-dir "$testdir"; then
    echo "Failed to create region"
    exit 1
fi
# Build the arg list for both dump and the client.
# This is improving, but still a bit hacky.  The port numbers here
# are the same as what DSC uses by default.  If either side changes, then
# the other will need to be update manually.
target_args="-t 127.0.0.1:8810 -t 127.0.0.1:8820 -t 127.0.0.1:8830"
dump_args+=" -d ${testdir}/8810 -d ${testdir}/8820 -d ${testdir}/8830"

if pgrep -fl -U "$(id -u)" "$cds"; then
    echo "Downstairs already running" >&2
    echo Run: pkill -f -U "$(id -u)" "$cds" >&2
    exit 1
fi

# Start all three downstairs
${cds} run -d "${testdir}/8810" -p 8810 &> "$ds_log_prefix"8810.txt &
ds0_pid=$!
${cds} run -d "${testdir}/8820" -p 8820 &> "$ds_log_prefix"8820.txt &
ds1_pid=$!
${cds} run -d "${testdir}/8830" -p 8830 &> "$ds_log_prefix"8830.txt &
ds2_pid=$!

os_name=$(uname)
if [[ "$os_name" == 'Darwin' ]]; then
    # stupid macos needs this to avoid popup hell.
    codesign -s - -f "$cds"
    codesign -s - -f "$ct"
fi

# Do initial volume population.
generation=1
echo "$ct with $target_args $dump_args $ds0_pid $ds1_pid $ds2_pid"
if ! ${ct} fill ${target_args} --verify-out "$verify_file" -q -g "$generation"
then
    echo "ERROR: Exit on initial fill"
    cleanup
    exit 1
fi
(( generation += 1))

# Start loop
for (( i = 0; i < 100; i += 1 )); do

    choice=$((RANDOM % 3))
    echo ""
    echo "Begin loop $i"
    echo "Downstairs to restart: $choice"

    # stop a downstairs and restart with lossy
    if [[ $choice -eq 0 ]]; then
        kill "$ds0_pid"
        wait "$ds0_pid" || true
        ${cds} run -d "${testdir}/8810" -p 8810 --lossy &> "$ds_log_prefix"8810.txt &
        ds0_pid=$!
    elif [[ $choice -eq 1 ]]; then
        kill "$ds1_pid"
        wait "$ds1_pid" || true
        ${cds} run -d "${testdir}/8820" -p 8820 --lossy &> "$ds_log_prefix"8820.txt &
        ds1_pid=$!
    else
        kill "$ds2_pid"
        wait "$ds2_pid" || true
        ${cds} run -d "${testdir}/8830" -p 8830 --lossy &> "$ds_log_prefix"8830.txt &
        ds2_pid=$!
    fi

    if ! ${ct} repair ${target_args} --verify-out "$verify_file" --verify-in "$verify_file" -c 30 -g "$generation"
    then
        echo "Exit on repair fail, loop: $i, choice: $choice"
        cleanup
        exit 1
    fi
    (( generation += 1))

    echo ""
    # Stop --lossy downstairs so it can't complete all its IOs
    if [[ $choice -eq 0 ]]; then
        kill "$ds0_pid"
        wait "$ds0_pid" || true
    elif [[ $choice -eq 1 ]]; then
        kill "$ds1_pid"
        wait "$ds1_pid" || true
    else
        kill "$ds2_pid"
        wait "$ds2_pid" || true
    fi

    # Did we get any mismatches?
    # We || true because dump will return non-zero when it finds
    # a mismatch
    echo "Current downstairs dump: da:${dump_args}"
    ${cds} dump ${dump_args} || true
    echo "On loop $i"

    echo ""
    # Start downstairs without lossy
    if [[ $choice -eq 0 ]]; then
        ${cds} run -d "${testdir}/8810" -p 8810 &> "$ds_log_prefix"8810.txt &
        ds0_pid=$!
    elif [[ $choice -eq 1 ]]; then
        ${cds} run -d "${testdir}/8820" -p 8820 &> "$ds_log_prefix"8820.txt &
        ds1_pid=$!
    else
        ${cds} run -d "${testdir}/8830" -p 8830 &> "$ds_log_prefix"8830.txt &
        ds2_pid=$!
    fi

    echo "Verifying data now"
    echo ${ct} verify ${target_args} --verify-out "$verify_file" --verify-in "$verify_file" --range -q -g "$generation" > "$test_log"
    if ! ${ct} verify ${target_args} --verify-out "$verify_file" --verify-in "$verify_file" --range -q -g "$generation" >> "$test_log" 2>&1
    then
        echo "Exit on verify fail, loop: $i, choice: $choice"
        echo "Check $test_log for details"
        cleanup
        exit 1
    fi
    (( generation += 1))

    echo "Loop: $i  Downstairs dump after verify (and repair):"
    ${cds} dump ${dump_args[@]}

done

duration=$SECONDS
printf "%d:%02d Test duration\n" $((duration / 60)) $((duration % 60))
echo "Test completed"
cleanup
