# Sourced by the NVIDIA builders (build_boot_image.sh, build_nvidia_driver_package.sh,
# build_nvidia_driver_layer.sh).
#
# Ubuntu publishes NVIDIA kernel modules built and signed by Canonical for each kernel ABI:
# linux-modules-nvidia-<branch>-open-<kernel> (the open modules, ready to load) and
# linux-modules-nvidia-<branch>-<kernel> (the proprietary ones, linked at install time from
# linux-objects-nvidia-* against the signatures in linux-signatures-nvidia-*). The kernel trusts
# Canonical's key, so with them the driver loads with Secure Boot on: no DKMS, no MOK enrolment.

# The branch of a driver package name: nvidia-driver-580-open -> 580.
nvidia_branch() {
    local branch="${1#nvidia-driver-}"
    printf '%s\n' "${branch%-open}"
}

# The signed module package of `driver` for `kernel` (an uname -r).
nvidia_signed_module_package() {
    local driver="$1" kernel="$2" branch
    branch="$(nvidia_branch "$driver")"
    if [[ "$driver" == *-open ]]; then
        printf 'linux-modules-nvidia-%s-open-%s\n' "$branch" "$kernel"
    else
        printf 'linux-modules-nvidia-%s-%s\n' "$branch" "$kernel"
    fi
}

# The proprietary module packages of `branch` for `kernel`: the late-linked modules, their
# objects and Canonical's signatures. Linking needs binutils and the kernel's headers (objtool,
# module.lds), present only while the package is configured.
nvidia_proprietary_module_packages() {
    local branch="$1" kernel="$2"
    printf '%s\n' "linux-modules-nvidia-$branch-$kernel" "linux-objects-nvidia-$branch-$kernel" \
        "linux-signatures-nvidia-$kernel" binutils "linux-headers-$kernel"
}

# The userspace of nvidia-driver-<branch>[-open], the same for both flavors, without its DKMS
# and kernel source packages.
nvidia_userspace_packages() {
    local branch="$1"
    printf '%s\n' "nvidia-kernel-common-$branch" "libnvidia-compute-$branch" \
        "nvidia-compute-utils-$branch" "nvidia-utils-$branch" "libnvidia-cfg1-$branch" \
        "libnvidia-gl-$branch" "libnvidia-extra-$branch" "libnvidia-decode-$branch" \
        "libnvidia-encode-$branch" "libnvidia-fbc1-$branch" "xserver-xorg-video-nvidia-$branch"
}

# Everything to install for `driver` with Canonical's signed modules on `kernel`.
nvidia_signed_packages() {
    local driver="$1" kernel="$2" branch
    branch="$(nvidia_branch "$driver")"
    nvidia_userspace_packages "$branch"
    if [[ "$driver" == *-open ]]; then
        nvidia_signed_module_package "$driver" "$kernel"
    else
        nvidia_proprietary_module_packages "$branch" "$kernel"
    fi
}
