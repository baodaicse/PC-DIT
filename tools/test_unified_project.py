#!/usr/bin/env python3
"""Read-only regression checks for the merged workspace and image pipeline."""
from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]


class UnifiedProjectTests(unittest.TestCase):
    def test_single_workspace_contains_current_engines(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
        members = workspace["workspace"]["members"]
        self.assertEqual(len(members), 20)
        names = []
        for member in members:
            path = (ROOT / member).resolve()
            self.assertTrue(path.is_relative_to(ROOT))
            package = tomllib.loads((path / "Cargo.toml").read_text())
            self.assertNotIn("workspace", package)
            names.append(package["package"]["name"])
        self.assertTrue({"dit-cli", "nvme-cli", "gpu-engine", "gpu-vulkan", "gpu-cuda", "gpu-cli", "dit-orchestrator", "dit-watchdog", "dit-chain"}.issubset(names))
        self.assertEqual(len(set(names)), 20)

    def test_no_previous_project_paths_in_active_code(self):
        old_path = re.compile(r"/home/hoang/(?:CPU-DIT-01|DIT-CPU-NVME-01)(?:/|\b)")
        candidates = [ROOT / "Makefile", ROOT / "Cargo.toml"]
        for directory in ("crates", "tools", "image"):
            candidates.extend(path for path in (ROOT / directory).rglob("*")
                              if path.is_file() and path.suffix in {".rs", ".sh", ".py", ".toml", ".conf"})
        for path in candidates:
            self.assertIsNone(old_path.search(path.read_text()), str(path))

    def test_image_always_includes_local_nvme_payload(self):
        builder = (ROOT / "tools/build_boot_image.sh").read_text()
        self.assertIn('EXTENSION_DIR="$ROOT_DIR/dist/cpu-nvme-extension-root"', builder)
        self.assertIn('"$ROOT_DIR/tools/build_image_extension.sh"', builder)
        self.assertIn('OUTPUT_IMAGE="$OUTPUT_DIR/dit-01-noble-amd64.img"', builder)
        self.assertLess(builder.index('"$ROOT_DIR/tools/build_image_extension.sh"'),
                        builder.index('    bootstrap_rootfs\n', builder.index('main()')))
        makefile = (ROOT / "Makefile").read_text()
        self.assertIn("shared-image: rufus-image", makefile)
        self.assertNotIn("NVME_PROJECT", makefile)
        extension = (ROOT / "tools/build_image_extension.sh").read_text()
        self.assertIn("opt/gpu-vram-dit/gpu-vram-dit", extension)
        self.assertIn("opt/dit-chain/dit-chain", extension)
        self.assertIn("-p dit-chain", extension)
        self.assertIn("usr/share/doc/dit-suite/COMBINED_STRESS_CHAIN_ANALYSIS.md", extension)
        self.assertIn('"$ROOTFS/opt/dit-chain/dit-chain"', builder)
        self.assertIn('"dit.combined"', extension)
        self.assertIn('"dit.combined"', builder)
        self.assertIn("COMBINED-F-IMPLEMENTED", extension)
        self.assertIn("usr/share/gpu-vram-dit/matrix_u32.spv", extension)
        self.assertIn("usr/share/doc/gpu-vram-dit/VALIDATION.md", extension)
        self.assertIn("usr/share/doc/dit-suite/UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md", extension)
        self.assertIn("usr/local/bin/dit-watchdog", extension)
        self.assertIn("usr/share/doc/dit-suite/RUNTIME_WATCHDOG.md", extension)
        self.assertIn('"gpu_pipeline": "GPU-G3-1"', extension)
        self.assertIn('gpu_cuda_kernel_sha256', extension)

    def test_image_carries_the_storage_tools(self):
        # FORMAT SSD needs sfdisk, which Ubuntu 24.04 ships in `fdisk`, not util-linux
        # (missing it failed with exit status 127 after the old table was wiped).
        builder = (ROOT / "tools/build_boot_image.sh").read_text()
        for package in ("fdisk", "ntfs-3g", "e2fsprogs"):
            self.assertIn(f"\n    {package}\n", builder)
        self.assertIn("verify_image_storage_tools\n", builder)
        for tool in ("sfdisk", "wipefs", "partx", "mkntfs", "mkfs.ext4", "ntfsfix"):
            self.assertIn(tool, builder.split("IMAGE_STORAGE_TOOLS=(", 1)[1].split(")", 1)[0])

    def test_image_tools_agree_on_filename(self):
        for name in ("build_boot_image.sh", "burn_usb.sh", "run_image_local.sh", "test_boot_image_qemu.sh"):
            self.assertIn("dit-01-noble-amd64.img", (ROOT / "tools" / name).read_text())

    def test_packaged_module_documents_exist(self):
        builder = (ROOT / "tools/build_image_extension.sh").read_text()
        paths = re.findall(r'\$project_dir/(docs/[^"\n]+)', builder)
        self.assertTrue(paths)
        for path in paths:
            self.assertTrue((ROOT / path).is_file(), path)


if __name__ == "__main__":
    unittest.main(verbosity=2)
