#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
staging_dir="$project_dir/dist/cpu-nvme-extension-root"
archive="$project_dir/dist/cpu-nvme-extension.tar.gz"

case "$staging_dir" in
    "$project_dir"/dist/cpu-nvme-extension-root) ;;
    *) printf 'unsafe extension staging path: %s\n' "$staging_dir" >&2; exit 2 ;;
esac

mkdir -p "$project_dir/dist" "$staging_dir"
find "$staging_dir" -mindepth 1 -delete

# Screens and reports carry the checkout's version (crates/dit-contracts/build.rs); build_boot_image.sh
# passes the one it computed, a direct run computes it here.
DIT_VERSION="${DIT_VERSION:-$("$project_dir/tools/dit-version.sh")}"
export DIT_VERSION
cargo build --release -p nvme-cli -p gpu-cli -p dit-orchestrator -p dit-watchdog -p dit-chain \
    -p dit-blackbox \
    --manifest-path "$project_dir/Cargo.toml"

install -D -m 0755 "$project_dir/target/release/cpu-nvme-dit" \
    "$staging_dir/opt/cpu-nvme-dit/cpu-nvme-dit"
install -D -m 0755 "$project_dir/target/release/dit-orchestrator" \
    "$staging_dir/usr/local/bin/dit-suite"
install -D -m 0755 "$project_dir/target/release/dit-watchdog" \
    "$staging_dir/usr/local/bin/dit-watchdog"
install -D -m 0755 "$project_dir/target/release/dit-rescue" \
    "$staging_dir/usr/local/bin/dit-rescue"
install -D -m 0755 "$project_dir/target/release/dit-blackbox" \
    "$staging_dir/usr/local/bin/dit-blackbox"
install -D -m 0755 "$project_dir/target/release/gpu-vram-dit" \
    "$staging_dir/opt/gpu-vram-dit/gpu-vram-dit"
install -D -m 0755 "$project_dir/target/release/dit-chain" \
    "$staging_dir/opt/dit-chain/dit-chain"
install -D -m 0644 "$project_dir/docs/COMBINED_STRESS_CHAIN_ANALYSIS.md" \
    "$staging_dir/usr/share/doc/dit-suite/COMBINED_STRESS_CHAIN_ANALYSIS.md"
install -D -m 0644 "$project_dir/image/extension/etc/dit-suite/cpu-nvme.conf" \
    "$staging_dir/etc/dit-suite/cpu-nvme.conf"
install -D -m 0644 "$project_dir/Cargo.lock" \
    "$staging_dir/usr/share/doc/cpu-nvme-dit/Cargo.lock"
install -D -m 0644 "$project_dir/docs/cpu-nvme/TASK_F_IMPLEMENTATION.md" \
    "$staging_dir/usr/share/doc/cpu-nvme-dit/OPERATOR.md"
install -D -m 0644 "$project_dir/docs/cpu-nvme/DATA_PRESERVATION.md" \
    "$staging_dir/usr/share/doc/cpu-nvme-dit/DATA_PRESERVATION.md"
install -D -m 0644 "$project_dir/docs/cpu-nvme/AUTO_STORAGE.md" \
    "$staging_dir/usr/share/doc/cpu-nvme-dit/AUTO_STORAGE.md"
install -D -m 0644 "$project_dir/docs/NVME_DEEP_PIPELINE.md" \
    "$staging_dir/usr/share/doc/cpu-nvme-dit/NVME_DEEP_PIPELINE.md"
install -D -m 0644 "$project_dir/docs/UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md" \
    "$staging_dir/usr/share/doc/dit-suite/UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md"
install -D -m 0644 "$project_dir/docs/RUNTIME_WATCHDOG.md" \
    "$staging_dir/usr/share/doc/dit-suite/RUNTIME_WATCHDOG.md"
install -D -m 0644 "$project_dir/docs/FAULT_CATCH_ROADMAP.md" \
    "$staging_dir/usr/share/doc/dit-suite/FAULT_CATCH_ROADMAP.md"
install -D -m 0644 "$project_dir/docs/gpu-vram/ROADMAP.md" \
    "$staging_dir/usr/share/doc/gpu-vram-dit/ROADMAP.md"
install -D -m 0644 "$project_dir/docs/gpu-vram/STATUS.md" \
    "$staging_dir/usr/share/doc/gpu-vram-dit/STATUS.md"
install -D -m 0644 "$project_dir/docs/gpu-vram/GPU_EF_IMPLEMENTATION.md" \
    "$staging_dir/usr/share/doc/gpu-vram-dit/GPU_EF_IMPLEMENTATION.md"
install -D -m 0644 "$project_dir/docs/gpu-vram/VALIDATION.md" \
    "$staging_dir/usr/share/doc/gpu-vram-dit/VALIDATION.md"
install -D -m 0644 "$project_dir/crates/gpu-vulkan/shaders/matrix_u32.comp" \
    "$staging_dir/usr/share/gpu-vram-dit/matrix_u32.comp"
install -D -m 0644 "$project_dir/crates/gpu-vulkan/shaders/matrix_u32.spv" \
    "$staging_dir/usr/share/gpu-vram-dit/matrix_u32.spv"
{
    printf 'DIT shared module dependency SBOM v1\nname\tversion\n'
    awk -F '"' '
        /^name = "/ { name=$2 }
        /^version = "/ && name != "" { print name "\t" $2; name="" }
    ' "$project_dir/Cargo.lock" | sort -u
} >"$staging_dir/usr/share/doc/cpu-nvme-dit/SBOM.tsv"

nvme_sha=$(sha256sum "$staging_dir/opt/cpu-nvme-dit/cpu-nvme-dit" | awk '{print $1}')
gpu_sha=$(sha256sum "$staging_dir/opt/gpu-vram-dit/gpu-vram-dit" | awk '{print $1}')
suite_sha=$(sha256sum "$staging_dir/usr/local/bin/dit-suite" | awk '{print $1}')
watchdog_sha=$(sha256sum "$staging_dir/usr/local/bin/dit-watchdog" | awk '{print $1}')
rescue_sha=$(sha256sum "$staging_dir/usr/local/bin/dit-rescue" | awk '{print $1}')
blackbox_sha=$(sha256sum "$staging_dir/usr/local/bin/dit-blackbox" | awk '{print $1}')
chain_sha=$(sha256sum "$staging_dir/opt/dit-chain/dit-chain" | awk '{print $1}')
shader_sha=$(sha256sum "$staging_dir/usr/share/gpu-vram-dit/matrix_u32.spv" | awk '{print $1}')
install -D -m 0644 "$project_dir/crates/gpu-cuda/kernels/gemm.cu" "$staging_dir/usr/share/gpu-vram-dit/gemm.cu"
install -D -m 0644 "$project_dir/crates/gpu-cuda/kernels/gemm.ptx" "$staging_dir/usr/share/gpu-vram-dit/gemm.ptx"
install -D -m 0644 "$project_dir/docs/gpu-vram/GPU_G3_CUDA_IMPLEMENTATION.md" "$staging_dir/usr/share/doc/gpu-vram-dit/GPU_G3_CUDA_IMPLEMENTATION.md"
cuda_sha=$(sha256sum "$staging_dir/usr/share/gpu-vram-dit/gemm.ptx" | awk '{print $1}')
version=$(awk -F '"' '/^version = "/ { print $2; exit }' "$project_dir/Cargo.toml")
mkdir -p "$staging_dir/etc/dit-suite/modules.d"
printf '%s\n' \
    '{' \
    '  "schema_version": 1,' \
    '  "extension_id": "dit.cpu-nvme",' \
    "  \"version\": \"$version\"," \
    '  "image_owner": "DIT-01",' \
    '  "modules": ["dit.cpu-ram", "dit.cpu-nvme", "dit.vga", "dit.cpu-power-platform", "dit.combined"],' \
    '  "launchers": {' \
    '    "control_center": "/usr/local/bin/dit-suite",' \
    '    "watchdog": "/usr/local/bin/dit-watchdog",' \
    '    "rescue_console": "/usr/local/bin/dit-rescue",' \
    '    "black_box": "/usr/local/bin/dit-blackbox",' \
    '    "cpu_nvme": "/opt/cpu-nvme-dit/cpu-nvme-dit",' \
    '    "gpu_vram": "/opt/gpu-vram-dit/gpu-vram-dit",' \
    '    "chain": "/opt/dit-chain/dit-chain"' \
    '  },' \
    '  "artifacts": {' \
    "    \"cpu-nvme-dit.sha256\": \"$nvme_sha\"," \
    "    \"gpu-vram-dit.sha256\": \"$gpu_sha\"," \
    "    \"dit-suite.sha256\": \"$suite_sha\"," \
    "    \"dit-watchdog.sha256\": \"$watchdog_sha\"," \
    "    \"dit-rescue.sha256\": \"$rescue_sha\"," \
    "    \"dit-blackbox.sha256\": \"$blackbox_sha\"," \
    "    \"dit-chain.sha256\": \"$chain_sha\"," \
    "    \"matrix_u32.spv.sha256\": \"$shader_sha\"" \
    '  },' \
    '  "interactive_storage_mode": "DATA_PRESERVING_FILE",' \
    '  "default_profile": "DEEP",' \
    '  "ui_contract": "DIT-UNIFIED-1",' \
    '  "combined_pipeline_status": "COMBINED-F-IMPLEMENTED",' \
    '  "gpu_pipeline": "GPU-G3-1",' \
    '  "gpu_backend": "CUDA (NVIDIA) / Vulkan (Intel, AMD)",' \
    "  \"gpu_cuda_kernel_sha256\": \"$cuda_sha\"," \
    "  \"gpu_shader_sha256\": \"$shader_sha\"," \
    '  "block_format": "DITDEEP2",' \
    '  "raw_write_ui_enabled": false,' \
    '  "sbom": "/usr/share/doc/cpu-nvme-dit/SBOM.tsv"' \
    '}' >"$staging_dir/etc/dit-suite/modules.d/cpu-nvme.json"

(
    cd "$staging_dir"
    sha256sum \
        opt/cpu-nvme-dit/cpu-nvme-dit \
        opt/gpu-vram-dit/gpu-vram-dit \
        usr/local/bin/dit-suite \
        usr/local/bin/dit-watchdog \
        usr/local/bin/dit-rescue \
        usr/local/bin/dit-blackbox \
        opt/dit-chain/dit-chain \
        usr/share/doc/dit-suite/COMBINED_STRESS_CHAIN_ANALYSIS.md \
        etc/dit-suite/cpu-nvme.conf \
        etc/dit-suite/modules.d/cpu-nvme.json \
        usr/share/doc/cpu-nvme-dit/Cargo.lock \
        usr/share/doc/cpu-nvme-dit/SBOM.tsv \
        usr/share/doc/cpu-nvme-dit/OPERATOR.md \
        usr/share/doc/cpu-nvme-dit/DATA_PRESERVATION.md \
        usr/share/doc/cpu-nvme-dit/AUTO_STORAGE.md \
        usr/share/doc/cpu-nvme-dit/NVME_DEEP_PIPELINE.md \
        usr/share/doc/dit-suite/UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md \
        usr/share/doc/dit-suite/RUNTIME_WATCHDOG.md \
        usr/share/doc/dit-suite/FAULT_CATCH_ROADMAP.md >MANIFEST.sha256
    sha256sum \
        usr/share/doc/gpu-vram-dit/ROADMAP.md \
        usr/share/doc/gpu-vram-dit/STATUS.md \
        usr/share/doc/gpu-vram-dit/GPU_EF_IMPLEMENTATION.md \
        usr/share/doc/gpu-vram-dit/VALIDATION.md \
        usr/share/gpu-vram-dit/matrix_u32.comp \
        usr/share/gpu-vram-dit/matrix_u32.spv \
        usr/share/gpu-vram-dit/gemm.cu \
        usr/share/gpu-vram-dit/gemm.ptx \
        usr/share/doc/gpu-vram-dit/GPU_G3_CUDA_IMPLEMENTATION.md >>MANIFEST.sha256
    sha256sum --check MANIFEST.sha256
)

archive_tmp="$archive.tmp"
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
    -C "$staging_dir" -czf "$archive_tmp" .
mv -f "$archive_tmp" "$archive"
sha256sum "$archive" >"$archive.sha256"

printf 'DIT shared extension ready\nRoot: %s\nArchive: %s\nSHA-256: %s\n' \
    "$staging_dir" "$archive" "$(awk '{print $1}' "$archive.sha256")"
