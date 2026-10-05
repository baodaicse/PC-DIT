#!/bin/bash
# The NVIDIA module flavor choice (image/rootfs/usr/local/lib/cpu-dit/nvidia-flavor.sh) on a fake
# sysfs: a fake modprobe binds each GPU by its generation and the flavor in updates/dkms (open:
# Turing and newer; proprietary: Maxwell to Ada, not Blackwell), as the real modules do.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FLAVOR_SCRIPT="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/nvidia-flavor.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat >"$WORK/modprobe" <<'EOF'
#!/bin/bash
# Fake modprobe: $SCENARIO/gpus lists "<pci address> <generation>"; $SCENARIO/held keeps nvidia.
sys="$NVIDIA_FLAVOR_SYS"
if [[ "$1" == -r ]]; then
    [[ "$2" == nvidia ]] || exit 0
    [[ -e "$SCENARIO/held" ]] && exit 1
    rm -rf "$sys/module/nvidia" "$sys/bus/pci/drivers/nvidia"
    exit 0
fi
[[ "$1" == nvidia ]] || exit 0
[[ -d "$sys/module/nvidia" ]] && exit 0
flavor="$(cat "$NVIDIA_FLAVOR_MODULES/updates/dkms/nvidia.ko")"
bound=0
while read -r address generation; do
    case "$flavor:$generation" in
        open:turing | open:blackwell | proprietary:maxwell | proprietary:pascal | proprietary:turing)
            mkdir -p "$sys/bus/pci/drivers/nvidia"
            ln -s "$sys/bus/pci/devices/$address" "$sys/bus/pci/drivers/nvidia/$address"
            bound=$((bound + 1)) ;;
    esac
done <"$SCENARIO/gpus"
(( bound > 0 )) || exit 1   # "NVRM: No NVIDIA GPU found": the module does not stay
mkdir -p "$sys/module/nvidia"
EOF
chmod +x "$WORK/modprobe"

fail() {
    printf 'NVIDIA_FLAVOR_TEST_FAIL: %s\n' "$*" >&2
    exit 2
}

# run <name> <expected flavor> <expected bound> <generation...>; options before the name:
# --no-proprietary (layer without it), --held (the open stack cannot be unloaded).
run() {
    local proprietary=1 held=0
    while [[ "$1" == --* ]]; do
        case "$1" in
            --no-proprietary) proprietary=0 ;;
            --held) held=1 ;;
        esac
        shift
    done
    local name="$1" expected="$2" expected_bound="$3"
    shift 3
    local scenario="$WORK/$name" kernel=7.0.0-30-generic
    mkdir -p "$scenario/sys/bus/pci/devices" "$scenario/modules/updates/dkms" \
        "$scenario/flavors/open/$kernel"
    printf 'open' >"$scenario/modules/updates/dkms/nvidia.ko"
    printf 'open' >"$scenario/flavors/open/$kernel/nvidia.ko"
    if (( proprietary )); then
        mkdir -p "$scenario/flavors/proprietary/$kernel"
        printf 'proprietary' >"$scenario/flavors/proprietary/$kernel/nvidia.ko"
    fi
    : >"$scenario/gpus"
    local slot=1 generation address
    for generation in "$@"; do
        address="0000:0$slot:00.0"
        mkdir -p "$scenario/sys/bus/pci/devices/$address"
        printf '0x10de\n' >"$scenario/sys/bus/pci/devices/$address/vendor"
        printf '0x030000\n' >"$scenario/sys/bus/pci/devices/$address/class"
        printf '%s %s\n' "$address" "$generation" >>"$scenario/gpus"
        slot=$((slot + 1))
    done
    # An Intel iGPU beside them is not counted.
    mkdir -p "$scenario/sys/bus/pci/devices/0000:00:02.0"
    printf '0x8086\n' >"$scenario/sys/bus/pci/devices/0000:00:02.0/vendor"
    printf '0x030000\n' >"$scenario/sys/bus/pci/devices/0000:00:02.0/class"
    (
        export SCENARIO="$scenario"
        export NVIDIA_FLAVOR_SYS="$scenario/sys"
        export NVIDIA_FLAVOR_ROOT="$scenario/flavors"
        export NVIDIA_FLAVOR_KERNEL="$kernel"
        export NVIDIA_FLAVOR_MODULES="$scenario/modules"
        export NVIDIA_FLAVOR_STATE="$scenario/state"
        export NVIDIA_FLAVOR_MODPROBE="$WORK/modprobe"
        export NVIDIA_FLAVOR_DEPMOD=true
        export NVIDIA_FLAVOR_MODPROBE_D="$scenario/modprobe.d"
        # shellcheck source=/dev/null
        . "$FLAVOR_SCRIPT"
        if (( held )); then
            "$NVIDIA_FLAVOR_MODPROBE" nvidia || true
            touch "$scenario/held"
        fi
        select_nvidia_flavor "$scenario/log"
        local chosen bound installed
        chosen="$(cat "$scenario/state" 2>/dev/null || printf none)"
        bound="$(nvidia_bound_count)"
        installed="$(cat "$scenario/modules/updates/dkms/nvidia.ko")"
        [[ "$chosen" == "$expected" ]] \
            || fail "$name: chose $chosen, expected $expected; log: $(tr '\n' ';' <"$scenario/log")"
        [[ "$bound" == "$expected_bound" ]] \
            || fail "$name: $bound GPU(s) driven, expected $expected_bound"
        if [[ "$expected" != none ]]; then
            [[ "$installed" == "$expected" ]] \
                || fail "$name: updates/dkms holds $installed, expected $expected"
        fi
        # nvidia-drm keeps its hands off the console with the proprietary modules only.
        local console="$scenario/modprobe.d/zz-dit-nvidia-console.conf"
        if [[ "$expected" == proprietary ]]; then
            grep -qx 'options nvidia_drm modeset=0 fbdev=0' "$console" \
                || fail "$name: nvidia-drm would take the console"
        elif [[ -e "$console" ]]; then
            fail "$name: the console override stayed with the open modules"
        fi
    )
}

run turing open 1 turing
run blackwell open 1 blackwell
run pascal proprietary 1 pascal
run maxwell proprietary 1 maxwell
run pascal-and-turing proprietary 2 pascal turing
run pascal-and-blackwell open 1 pascal blackwell
run --no-proprietary pascal-without-proprietary open 0 pascal
run --held held-open-stack open 1 pascal turing
run no-nvidia none 0
grep -q 'nvidia_flavor open drives 0 of 1' "$WORK/pascal/log" || fail 'pascal: log lacks the open attempt'
grep -q 'nvidia_flavor proprietary drives 1 of 1' "$WORK/pascal/log" \
    || fail 'pascal: log lacks the proprietary result'
grep -q 'back to open' "$WORK/pascal-and-blackwell/log" || fail 'mixed Blackwell: no way back'
grep -q 'could not switch' "$WORK/held-open-stack/log" || fail 'held stack: no note'

printf 'NVIDIA module flavor tests: PASS (open, proprietary, mixed, fallback)\n'
