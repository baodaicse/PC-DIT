SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
PROJECT_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
IMAGE_NAME ?= dit-01-noble-amd64.img
IMAGE := $(PROJECT_ROOT)/dist/$(IMAGE_NAME)
# Driver baked into the prebuilt layer / ISO; the other NVIDIA targets keep requiring it explicitly.
LAYER_DRIVER_PACKAGE := $(or $(NVIDIA_DRIVER_PACKAGE),nvidia-driver-580-open)
NVIDIA_LAYER := $(PROJECT_ROOT)/dist/dit-nvidia-driver.squashfs
SSDS ?= 2
FRAMES ?= 1
.DEFAULT_GOAL := help
.NOTPARALLEL:
.PHONY: test-power-campaign test-power-image
.PHONY: gpu-g12-check test-gpu-isa
.PHONY: nvidia-driver-layer iso iso-only iso-burn-usb
.PHONY: gpu-g3-check gpu-cuda-kernel gpu-h1-check gpu-h2-check gpu-h3-check gpu-h5-check gpu-h6-check gpu-h-check

# GPU-H1: contract/measurement gates; DIT_GPU_HARDWARE=1 also retains the baseline matrix.
gpu-h1-check:
	tools/run_gpu_h1_checks.sh

# GPU-H2: low-copy verifier, hash policy, corpus rotation; DIT_GPU_HARDWARE=1 adds the A/B matrix.
gpu-h2-check:
	tools/run_gpu_h2_checks.sh

# GPU-H3: device/host slot pools, budget backoff, quarantine; DIT_GPU_HARDWARE=1 adds hardware gates.
gpu-h3-check:
	tools/run_gpu_h3_checks.sh

# GPU-H5: load controller, worker scaling, coverage floor; DIT_GPU_HARDWARE=1 adds hardware gates.
gpu-h5-check:
	tools/run_gpu_h5_checks.sh

# GPU-H6: overlapped transfer lane, GPU-local VRAM workload, profiles, fair verifier queue;
# DIT_GPU_HARDWARE=1 adds hardware gates.
gpu-h6-check:
	tools/run_gpu_h6_checks.sh

# GPU-H: every H1-H6 gate in order (H4a/H4b/H7/H8 have no gate yet). DIT_GPU_HARDWARE=1 adds
# every task's hardware gates; each retains its own evidence directory.
gpu-h-check: gpu-h1-check gpu-h2-check gpu-h3-check gpu-h5-check gpu-h6-check

gpu-g3-check:
	cargo test -p gpu-engine -p gpu-vulkan -p gpu-cuda -p gpu-cli
	cargo clippy -p gpu-cuda -p gpu-cli --all-targets -- -D warnings
	@if [[ "$${DIT_GPU_HARDWARE:-0}" == 1 ]]; then \
	  cargo test -p gpu-cuda --test g3 -- --ignored --nocapture; \
	  cargo test -p gpu-vulkan -- --ignored --test-threads=1; \
	fi

# Optional regeneration only. Normal Cargo builds use checked-in PTX.
gpu-cuda-kernel:
	@test -n "$(NVRTC_LIBRARY)" || (echo 'Set NVRTC_LIBRARY to CUDA 12.9 libnvrtc.so.12'; exit 2)
	mkdir -p build/cuda-toolchain
	cc -O2 tools/compile_cuda_kernel.c -ldl -o build/cuda-toolchain/compile-kernel
	build/cuda-toolchain/compile-kernel '$(NVRTC_LIBRARY)' crates/gpu-cuda/kernels/gemm.cu crates/gpu-cuda/kernels/gemm.ptx
	build/cuda-toolchain/compile-kernel '$(NVRTC_LIBRARY)' crates/gpu-cuda/kernels/tensor.cu crates/gpu-cuda/kernels/tensor.ptx compute_80
	sha256sum crates/gpu-cuda/kernels/gemm.ptx crates/gpu-cuda/kernels/tensor.ptx
.PHONY: help doctor fmt check test test-ui test-seed-validator release extension image shared-image rufus-image nvidia-image verify-image image-info burn-usb nvidia-burn-usb nvidia-driver-package nvidia-driver-burn-usb preview-menu preview-test-ui host-check validation-check campaign smoke task-a-check task-b-check task-c-check task-d-check task-f-check gpu-a-check gpu-check gpu-ef-check qemu-smoke run-image

help:
	@printf '%s\n' 'DIT-01 — CPU-RAM + CPU-SSD + GPU-VRAM, one workspace and image' \
	  'make test              Unified Rust + CPU UI/boot/seed + project structure tests' \
	  'make release           Compile CPU-RAM, CPU-SSD, GPU-VRAM and the control center' \
	  'make shared-image      Build all current modules into one Rufus/DD image (alias: rufus-image)' \
	  'make nvidia-image NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open' \
	  '                       Build a separate proprietary NVIDIA image with full telemetry support' \
	  'make nvidia-burn-usb   Burn the NVIDIA-profile image to USB' \
	  'make nvidia-driver-package NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open' \
	  '                       Newer alternative: driver package for the generic image (needs make image first)' \
	  'make nvidia-driver-burn-usb  Burn the generic image + stage the driver package above onto its DIT-RESULTS partition' \
	  'make iso               Rebuild the image, then a hybrid ISO with the NVIDIA driver prebuilt (no install at boot)' \
	  'make iso-only          Same ISO from the current image (rebuilds the driver layer only if stale)' \
	  'make iso-burn-usb      Write the ISO raw to a USB (select, confirm erase, read back and verify)' \
	  '                       Driver: NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open, default nvidia-driver-580-open' \
	  'make verify-image      Verify the current unified image' \
	  'make burn-usb          Select a USB, explicitly confirm erase/write/verify' \
	  'make preview-menu      Show shared menu only; no test or reboot' \
	  'make preview-test-ui SSDS=2 FRAMES=80  Synthetic NVMe UI, no SSD writes' \
	  'make host-check        CPU-RAM bounded host smoke + software gates, no image' \
	  'make test-power-campaign Seeded power replay + watchdog host smoke' \
	  'make test-power-image  Power campaign in QEMU, snapshot disk, direct kernel' \
	  'make task-f-check      Module integration/extension gates, no image/QEMU' \
	  'make gpu-a-check       GPU-A compatibility/mock regression gate' \
	  'make gpu-check         GPU-A..D gates + bounded Vulkan host smoke'
	@printf '%s\n' 'make gpu-ef-check      GPU-E/F shared UI/multi/image-extension gates; no image/QEMU'

doctor:
	@for tool in cargo make sha256sum sudo; do command -v "$$tool" >/dev/null || { echo "Missing $$tool"; exit 1; }; done

fmt:
	cargo fmt --all

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

test: check
	cargo test --workspace -- --test-threads=1
	cargo build --workspace
	./tools/test_ui.sh '$(PROJECT_ROOT)/target/debug/dit-cli'
	./tools/test_boot_policy.sh
	./tools/test_nvidia_flavor.sh
	bash -n ./tools/burn_usb.sh
	./tools/burn_usb.sh --list >/dev/null
	python3 ./tools/test_seed_validator.py
	python3 ./tools/test_unified_project.py

# v1.2 C13: gpu-vram-dit's v3/v4 builds give the baseline's corpora, bit for bit.
test-gpu-isa:
	cargo build --release -p gpu-cli
	./tools/test_gpu_isa.sh

test-ui:
	cargo build -p dit-cli -p dit-watchdog
	./tools/test_ui.sh '$(PROJECT_ROOT)/target/debug/dit-cli'
	cargo test -p dit-ui-core

test-power-campaign:
	cargo build -p dit-cli -p dit-watchdog
	bash tools/test_power_campaign.sh

test-power-image: verify-image
	bash tools/test_power_image_qemu.sh '$(IMAGE)'

test-seed-validator:
	python3 ./tools/test_seed_validator.py

release:
	cargo build --release --workspace

extension:
	./tools/build_image_extension.sh

image:
	./tools/build_boot_image.sh

shared-image: rufus-image

rufus-image: test image verify-image
	@printf '\nDIT-01 RUFUS IMAGE READY\n%s\nUse Rufus DD/raw mode.\n' '$(IMAGE)'

nvidia-image: test
	@test -n '$(NVIDIA_DRIVER_PACKAGE)' || { echo 'Set NVIDIA_DRIVER_PACKAGE (example: nvidia-driver-NNN-open)'; exit 2; }
	NVIDIA_DRIVER_PACKAGE='$(NVIDIA_DRIVER_PACKAGE)' ./tools/build_boot_image.sh
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check 'dit-01-noble-amd64-nvidia.img.sha256'
	@printf '\nDIT-01 NVIDIA RUFUS IMAGE READY\n%s\nUse Rufus DD/raw mode; Secure Boot may stay on.\n' \
	  '$(PROJECT_ROOT)/dist/dit-01-noble-amd64-nvidia.img'

verify-image:
	@test -s '$(IMAGE)' && test -s '$(IMAGE).sha256' && test -s '$(IMAGE).manifest.json'
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check '$(IMAGE_NAME).sha256'

image-info: verify-image
	@stat -c 'File: %n%nSize: %s bytes' '$(IMAGE)'
	@sed -n '1,120p' '$(IMAGE).manifest.json'

burn-usb: verify-image
	./tools/burn_usb.sh --image '$(IMAGE)'

nvidia-burn-usb:
	@test -s '$(PROJECT_ROOT)/dist/dit-01-noble-amd64-nvidia.img' \
	  && test -s '$(PROJECT_ROOT)/dist/dit-01-noble-amd64-nvidia.img.sha256' \
	  && test -s '$(PROJECT_ROOT)/dist/dit-01-noble-amd64-nvidia.img.manifest.json'
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check 'dit-01-noble-amd64-nvidia.img.sha256'
	./tools/burn_usb.sh --image '$(PROJECT_ROOT)/dist/dit-01-noble-amd64-nvidia.img'

# Newer alternative to nvidia-image/nvidia-burn-usb: one generic image (make image) plus an
# NVIDIA driver package installed at boot time instead of a second baked rootfs/image. See
# docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md. Needs 'make image' already built (exact kernel match)
# plus network+sudo to resolve the driver's dependency closure.
nvidia-driver-package:
	@test -n '$(NVIDIA_DRIVER_PACKAGE)' || { echo 'Set NVIDIA_DRIVER_PACKAGE (example: nvidia-driver-580-open)'; exit 2; }
	NVIDIA_DRIVER_PACKAGE='$(NVIDIA_DRIVER_PACKAGE)' ./tools/build_nvidia_driver_package.sh
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check 'dit-nvidia-driver-package.tar.gz.sha256'

# Burns the generic image (same as burn-usb) and stages the driver package built above onto its
# DIT-RESULTS partition; nvidia-driver-install.sh on the booted image installs it if it finds an
# NVIDIA GPU. Run 'make nvidia-driver-package' first (or set NVIDIA_DRIVER_PACKAGE to build it now).
nvidia-driver-burn-usb: verify-image
	@test -s '$(PROJECT_ROOT)/dist/dit-nvidia-driver-package.tar.gz' \
	  || $(MAKE) nvidia-driver-package
	./tools/burn_usb.sh --image '$(IMAGE)' \
	  --nvidia-driver-package '$(PROJECT_ROOT)/dist/dit-nvidia-driver-package.tar.gz'

# Prebuilt driver layer for the current generic image: the driver is installed and its DKMS modules
# compiled once, here, instead of at every boot. Pairs with exactly one image build (image_id).
nvidia-driver-layer: verify-image
	NVIDIA_DRIVER_PACKAGE='$(LAYER_DRIVER_PACKAGE)' ./tools/build_nvidia_driver_layer.sh
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check 'dit-nvidia-driver.squashfs.sha256'

# Hybrid ISO (BIOS + UEFI) = the generic image + the driver layer above in /live + an appended
# DIT-RESULTS partition. Write it raw to a USB stick (Rufus DD mode, dd, Etcher).
iso: test image
	$(MAKE) iso-only

iso-only: verify-image
	@image_id=$$(awk -F'"' '/"image_id":/ { print $$4 }' '$(IMAGE).manifest.json'); \
	 layer_id=$$(awk -F'"' '/"image_id":/ { print $$4 }' '$(NVIDIA_LAYER).manifest.json' 2>/dev/null || true); \
	 layer_driver=$$(awk -F'"' '/"nvidia_driver_package":/ { print $$4 }' '$(NVIDIA_LAYER).manifest.json' 2>/dev/null || true); \
	 if [[ "$$layer_id" != "$$image_id" || "$$layer_driver" != '$(LAYER_DRIVER_PACKAGE)' ]]; then \
	   $(MAKE) nvidia-driver-layer; \
	 fi
	./tools/build_iso.sh
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check 'dit-01-noble-amd64-nvidia.iso.sha256'

# Same raw write + SHA-256 read-back as burn-usb; the ISO carries its own DIT-RESULTS partition.
iso-burn-usb:
	@cd '$(PROJECT_ROOT)/dist' && sha256sum --check 'dit-01-noble-amd64-nvidia.iso.sha256'
	./tools/burn_usb.sh --image '$(PROJECT_ROOT)/dist/dit-01-noble-amd64-nvidia.iso'

preview-menu: release
	target/release/dit-orchestrator render-menu \
	  --cpu-ui '$(PROJECT_ROOT)/image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh' \
	  --nvme-binary '$(PROJECT_ROOT)/target/release/cpu-nvme-dit' \
	  --gpu-binary '$(PROJECT_ROOT)/target/release/gpu-vram-dit'

gpu-a-check:
	./tools/run_gpu_a_checks.sh

gpu-check:
	./tools/run_gpu_bcd_checks.sh

gpu-ef-check:
	./tools/run_gpu_ef_checks.sh

gpu-g12-check:
	bash ./tools/run_gpu_g12_checks.sh

preview-test-ui:
	cargo run -q -p dit-ui-core --example preview -- '$(SSDS)' '$(FRAMES)'

campaign:
	cargo run -q -p nvme-cli -- campaign --operations 16

smoke:
	cargo run -q -p nvme-cli -- simulate --duration-seconds 60 --operations 16

validation-check: test release
	./tools/run_negative_campaign.sh '$(PROJECT_ROOT)/target/release/dit-cli'

host-check: validation-check
	./tools/run_host_smoke.sh '$(PROJECT_ROOT)/target/release/dit-cli'

task-a-check:
	./tools/run_task_a_checks.sh
task-b-check:
	./tools/run_task_b_checks.sh
task-c-check:
	./tools/run_task_c_checks.sh
task-d-check:
	./tools/run_task_d_checks.sh
task-f-check:
	./tools/run_task_f_checks.sh

# Optional, never part of the default validation/build flow.
qemu-smoke: verify-image
	./tools/test_boot_image_qemu.sh '$(IMAGE)'
run-image: verify-image
	./tools/run_image_local.sh uefi
