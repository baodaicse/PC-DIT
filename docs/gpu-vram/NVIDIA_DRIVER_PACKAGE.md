# Gói driver NVIDIA cài lúc boot — thay thế cho `nvidia-image`

Trạng thái 2026-09-22: đóng gói + cài offline đã chạy thật và PASS trong chroot (257 gói, DKMS
build ra 5 module `.ko`). **Boot thật đầu tiên trên RTX 5060 lộ ra driver chạy `nouveau` thay vì
nvidia** — nguyên nhân và cách sửa (`module_blacklist=nouveau` ở grub.cfg) ghi ở dưới. **Boot thật
lần 2 (2026-09-23): driver không được cài** vì `/run` noexec — xem mục "Lỗi `/run` noexec"; đã sửa và
chạy thử trong môi trường mô phỏng live boot, **chưa build lại image + burn + boot lại trên phần cứng**. `nvidia-image`/`nvidia-burn-usb` **vẫn giữ
nguyên, không xoá**; đây là một đường đi song song, không bắt buộc.

## Vấn đề

`make nvidia-image` build hẳn một rootfs/image `dit-01-noble-amd64-nvidia.img` riêng: cài driver
NVIDIA bằng apt ngay trong lúc build, tách biệt hoàn toàn với image generic (dùng driver mở).
Hệ quả: hai artifact riêng, cập nhật cái này không cập nhật cái kia, và mỗi lần cần image NVIDIA
phải build lại toàn bộ rootfs.

## Thiết kế mới

Chỉ **một** image (generic, `make image`/`shared-image`, không đổi gì so với trước). Driver NVIDIA
đóng thành một **gói rời**, burn cùng lên USB, và được cài **lúc boot** nếu máy có GPU NVIDIA thật.

1. **`tools/build_nvidia_driver_package.sh`** (`make nvidia-driver-package NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open`).
   Dùng lại đúng rootfs generic mà `make image` đã build (`build/boot-image/rootfs-noble-amd64`) để
   biết chính xác phiên bản kernel image đó chạy, rồi `apt-get install --download-only` bên trong
   chroot của rootfs đó để lấy trọn bộ closure dependency (driver + `dkms` + `build-essential` +
   đúng gói `linux-headers-<phiên bản kernel>`) mà **không** cài gì vào rootfs (không đụng vào image
   generic). Đóng gói cùng `install.sh` (cài offline bằng `dpkg -i` + `dpkg --configure -a`, không
   cần mạng/apt source lúc cài) và file blacklist nouveau, cộng `manifest.json` + `MANIFEST.sha256`
   cho từng file, nén thành `dist/dit-nvidia-driver-package.tar.gz` + `.sha256`.
2. **`tools/burn_usb.sh --nvidia-driver-package FILE`** (`make nvidia-driver-burn-usb`). Burn image
   generic như `burn-usb` bình thường, sau đó mount phân vùng **DIT-RESULTS** đã có sẵn trên chính
   USB đó (vốn dùng để giữ kết quả test qua các lần boot) và chép gói + `.sha256` vào
   `nvidia-driver/` trên đó, đọc lại để xác nhận. Không tạo phân vùng mới, không đụng bảng phân
   vùng — chỉ ghi file vào một phân vùng ghi-được đã tồn tại từ trước.
3. **`image/rootfs/usr/local/lib/cpu-dit/nvidia-driver-install.sh`** (baked vào **cả hai** flavor
   image, chạy ngay trước `driver-probe.sh` trong `boot-runner.sh`). Best-effort, không bao giờ làm
   fail boot:
   - Bỏ qua ngay nếu đây là flavor NVIDIA đã baked sẵn (`/etc/cpu-dit/nvidia-driver-profile` tồn
     tại), nếu máy không có GPU NVIDIA (`lspci -nn -d 10de:`), hoặc nếu `nvidia-smi` đã chạy được
     (đã cài từ trước trong phiên này).
   - Mount DIT-RESULTS (dùng lại `find_result_partition`/`mount_result_partition`, nay tách ra
     `partition-lookup.sh` dùng chung với `boot-runner.sh`), tìm gói, xác thực sha256 của cả file
     nén lẫn từng file bên trong (`MANIFEST.sha256`) trước khi chạm vào bất cứ thứ gì.
   - Chạy `install.sh` đã đóng gói, `modprobe` `nvidia`/`nvidia_modeset`/`nvidia_drm`/`nvidia_uvm`,
     rồi kiểm `nvidia-smi`. (Không cần gỡ `nouveau`: nó bị tắt hẳn trên mọi flavor, xem mục dưới;
     nếu vì lý do nào đó vẫn thấy `nouveau` đang chạy, script dừng với
     `failed_nouveau_loaded_despite_blacklist` thay vì cố gỡ.)
   - Ghi kết quả vào `/run/cpu-dit-nvidia-driver-runtime-installed` (một trong
     `skipped_*`/`failed_*`/`installed_and_working`/`installed_but_not_working`) và log đầy đủ vào
     `/run/cpu-dit-nvidia-driver-install.txt`.
4. **`driver-probe.sh`** có ba nhánh: `nvidia_baked` (flavor build sẵn), `nvidia_runtime` (marker
   trên tồn tại — probe bộ module `nvidia*`), và `open nouveau=disabled` (không có driver NVIDIA
   nào; Intel/AMD vẫn dùng driver mở như cũ). Luôn ghi thêm `nouveau_loaded no` (hoặc
   `UNEXPECTED` nếu có gì đó vượt qua được blacklist).

**Sửa 2026-09-23 — log không sống sót qua profile `menu`.** Cả hai log trên chỉ nằm trong `/run`
(tmpfs), và `boot-runner.sh` chỉ copy chúng vào thư mục kết quả của phiên chạy ở **nhánh profile
trực tiếp** (`cpu_dit_profile=safe/full/...`, dùng cho CPU-RAM); với profile `menu` (đường dùng để
vào GPU-VRAM qua `dit-suite`), `boot-runner.sh` `exec` thẳng vào `dit-suite` ngay sau khi hai script
trên chạy xong — đoạn copy phía sau **không bao giờ chạy tới**. Phát hiện bằng dữ liệu thật: một USB
đã test xong trên RTX 5060 không có `nvidia-driver-install.txt`/`driver-probe.txt` nào trong
`runs/`. Đã sửa: mỗi script tự chép log của mình vào `/mnt/cpu-dit-results/boot/` (ghi đè mỗi lần
boot) ngay khi kết thúc, best-effort, nếu DIT-RESULTS đang mount tại thời điểm đó (thường có, vì
`nvidia-driver-install.sh` đã mount nó để tìm gói trước khi `driver-probe.sh` chạy).

Sống trên hạ tầng USB đã có sẵn (DIT-BOOT/DIT-RESULTS, `image-id` khớp) nên không cần đổi bảng
phân vùng hay format lại gì trên image; USB không có gói vẫn boot y hệt như trước (no-op nhanh).

## Race `nouveau` — phát hiện thật trên RTX 5060, đã sửa

Lượt đầu tiên chỉ gỡ `nouveau` bằng `modprobe -r` **sau khi** hệ thống đã boot gần xong — nhưng udev
thường tự động nạp `nouveau` (theo modalias PCI) sớm hơn nhiều so với bất kỳ service nào của DIT-01
chạy, nên `nouveau` đã bind vào card **trước khi** `nvidia-driver-install.sh` kịp chạy, và
`modprobe -r` không gỡ được. Đúng như dự đoán ban đầu: build/cài offline trong chroot (không có GPU
thật) không tái hiện được race này — phải chờ chạy trên phần cứng thật mới lộ ra. Đo được thật trên
RTX 5060 (2026-09-22): UI nhận diện driver đang chạy là `nouveau`.

**Đã sửa tận gốc: `nouveau` bị tắt hẳn trên mọi flavor image**, ở ba tầng:

1. **Kernel cmdline** — `module_blacklist=nouveau` trên **mọi** entry của `image/boot/grub/grub.cfg`
   (cả hai flavor dùng chung file này). Kernel kiểm tra danh sách này ngay trong `load_module()`,
   nên nó từ chối **mọi** lần nạp — cả udev coldplug lẫn một lệnh `modprobe nouveau` tường minh.
   *(Đính chính 2026-09-23: bản ghi trước ở đây nói tham số này "không chặn modprobe tường minh" —
   sai; đó là hành vi của `modprobe.blacklist=`/`blacklist` trong modprobe.d, không phải của
   `module_blacklist=`.)*
2. **modprobe.d** — `/etc/modprobe.d/dit-blacklist-nouveau.conf` (`blacklist nouveau`,
   `options nouveau modeset=0`, `install nouveau /bin/false`) baked vào rootfs của **mọi** flavor bởi
   `disable_nouveau()` trong `build_boot_image.sh` — chặn cả khi ai đó boot bằng một cmdline tuỳ
   chỉnh thiếu tham số trên. Đã kiểm bằng `modprobe -n -v` (dry-run): `nouveau.ko` bị thay bằng
   `install /bin/false`, không bao giờ được `insmod`.
3. **initramfs** — `disable_nouveau()` chạy `update-initramfs` sau khi ghi file trên, nên initramfs
   mang cùng quy tắc.

`driver-probe.sh` không còn thử `probe_module nouveau` (đã là code chết sau tầng 1). Đây chính là
cách `nvidia-image` baked flavor đã làm từ đầu (blacklist trước khi boot lần đầu) — giờ áp dụng cho
generic image luôn. `tools/test_boot_policy.sh` kiểm cả ba tầng và kiểm `driver-probe.sh` không nạp
`nouveau`.

**Hệ quả cần biết:** trên máy có GPU NVIDIA mà **không** có gói driver NVIDIA (không burn gói, hoặc
dùng image generic cũ) thì card NVIDIA **không có kernel driver nào** — không có Vulkan ICD phần cứng
cho nó, GPU-VRAM sẽ báo INVALID (chỉ thấy llvmpipe, vốn bị từ chối) thay vì chạy trên `nouveau` như
trước. Muốn test card NVIDIA thì dùng `make nvidia-driver-burn-usb` (hoặc `nvidia-image`). Intel/AMD
không bị ảnh hưởng.

**Chưa xác nhận lại trên phần cứng sau khi sửa** — cần build lại image (`make image`, vì grub.cfg và
modprobe.d nằm trong ảnh), build lại gói (`make nvidia-driver-package`, vì file blacklist trong gói
đã đổi), burn lại USB và boot thử trên RTX 5060 để đóng hẳn mục này.

## Lỗi `/run` noexec — boot thật lần 2 (2026-09-23), đã sửa

Image mới (nouveau đã tắt) boot trên máy RTX 5050/5060: card NVIDIA **không có driver nào**, 3 lần chạy
GPU-VRAM đều dừng/INVALID, không có report. Không có log (image build lúc 16:01, trước bản sửa lưu log
vào `boot/` lúc 16:08). Manh mối: PID của worker rất thấp (820–1075) — nếu dpkg ~250 gói + DKMS compile
đã chạy thì PID phải lên hàng chục nghìn. Tức là script **bỏ cuộc sớm, không cài gì**.

Nguyên nhân: initramfs của image mount `/run` là `tmpfs` **`noexec`**, giới hạn **10% RAM**
(`/usr/share/initramfs-tools/init`: `mount -t tmpfs -o "nodev,noexec,nosuid,size=${RUNSIZE:-10%}"`).
Script giải nén gói vào `/run/dit-nvidia-driver/`, nên `[[ -x install.sh ]]` sai → dừng ngay với
`failed_missing_install_script` — trên **mọi** máy. Đã tái hiện bằng đúng gói trên USB và một tmpfs
noexec. Lần test chroot trước đã **bỏ lọt** lỗi này vì chạy `install.sh` từ `/root`, không phải `/run`
— tức là test đó chỉ chứng minh `install.sh` đúng, không chứng minh script chạy lúc boot đúng.

Đã sửa (`nvidia-driver-install.sh`):
- Giải nén vào `/var/tmp/dit-nvidia-driver` (overlay gốc: cho exec, tmpfs lớn hơn của live-boot), gọi
  `sh install.sh` thay vì dựa vào bit exec; kiểm dung lượng trước (`failed_no_space_for_install` nếu
  trống < 4× kích thước gói); xoá `pkgs/` sau khi cài để trả RAM.
- `DEBIAN_FRONTEND=noninteractive` và stdin `/dev/null` (service chạy không có tty).
- In trạng thái ra console (cài mất vài phút, trước đây màn hình im lặng); luôn lưu log vào
  `DIT-RESULTS/boot/` kể cả khi bỏ qua sớm; khi thất bại, chép thêm `make.log` của DKMS và dòng
  `NVRM` trong `dmesg` vào log.

**Test lại đúng cách lần này** — chạy chính `nvidia-driver-install.sh` (không phải riêng `install.sh`),
trong bản sao sạch của rootfs image, `/run` mount **đúng như initramfs** (tmpfs noexec 10%), dùng **đúng
gói trên USB**: chạy hết không can thiệp trong 183 s, cài đủ gói, DKMS build đủ 5 module, console hiện
trạng thái, log nằm ở `boot/`, `/run` chỉ dùng 204 KiB. Kết quả cuối `installed_but_not_working` là do
sandbox: `modprobe` gặp driver 580.173 của máy host đã nạp sẵn và `nvidia-smi` 580.178 báo "API
mismatch" — trên máy thật (kernel 7.0.0-30, chưa có driver NVIDIA nào) không có xung đột này. Bước
nạp module vào GPU thật vẫn chỉ xác nhận được trên phần cứng.

## Driver dựng sẵn + ISO (2026-09-23) — không cài lúc boot nữa

Cài lúc boot chạy đúng nhưng chậm: mỗi lần boot phải `dpkg` ~250 gói rồi compile DKMS (183 s trong
mô phỏng, lâu hơn trên máy yếu). Nay driver được cài **một lần lúc build**:

1. **`tools/build_nvidia_driver_layer.sh`** (`make nvidia-driver-layer`). Mount overlay lên rootfs
   generic (rootfs gốc không bị sửa), chạy chính `install.sh` của gói offline bên trong, DKMS build
   module cho đúng kernel của image, `depmod`, rồi nén **chỉ phần thay đổi** thành
   `dist/dit-nvidia-driver.squashfs` (nvidia-driver-580-open: 509 MiB). Bỏ phần chỉ dùng lúc build
   (`/usr/src`, cây DKMS, doc/man). Build xong thì kiểm `nvidia.ko` có mặt và `modules.alias` có
   alias PCI của NVIDIA, tức là udev sẽ tự nạp driver khi boot. Lớp này ghi
   `/etc/cpu-dit/nvidia-driver-profile` nên `nvidia-driver-install.sh` bỏ qua ngay
   (`skipped_baked_nvidia_flavor`) và `driver-probe.sh` đi nhánh `nvidia_baked`, y như
   `nvidia-image`. Lớp chỉ khớp với **đúng một** image build: `image_id` của nó nằm trong manifest.
2. **`tools/build_iso.sh`** (`make iso` / `make iso-only`). Chép payload boot từ DIT-BOOT của `.img`
   đã verify (bằng mtools, không mount), từ chối nếu `image_id` của lớp khác image, đặt lớp vào
   `/live/nvidia-driver.squashfs`. Khi boot, live-boot xếp các `/live/*.squashfs` theo thứ tự tên,
   lớp sau nằm trên. Tiếp theo `grub-mkrescue` tạo hybrid ISO boot được cả BIOS lẫn UEFI, volume id
   `DIT-BOOT`, và gắn thêm một phân vùng FAT32 **DIT-RESULTS** (mặc định 1024 MiB, `ISO_RESULT_MIB`)
   đã đóng dấu `CPU-DIT-RESULTS.ID`. Kết quả: `dist/dit-01-noble-amd64-nvidia.iso` + `.sha256` +
   `.manifest.json`.
3. **`partition-lookup.sh`** nhận thêm bố cục ISO. Khi ghi ra USB, volume ISO9660 nằm trên **cả
   ổ** (TYPE `disk`), còn DIT-RESULTS là phân vùng con của chính ổ đó. Phân vùng HFS+ của
   xorriso cũng mang nhãn DIT-BOOT nên bị loại trừ tường minh. Boot dạng CD (máy ảo, BMC virtual
   media) vẫn chạy nhưng không lưu kết quả.

Do `toram` chép **toàn bộ** thiết bị boot vào RAM, ISO có lớp NVIDIA làm mọi máy tốn thêm khoảng
0,5 GB RAM, kể cả máy không có NVIDIA. Đây là đánh đổi đã chọn: một file ISO duy nhất. Trên máy
không có NVIDIA, lớp này vô hại (nhánh `nvidia_baked` thử `modprobe nvidia` và bị từ chối vì không
có thiết bị).

**Menu boot hiện build nào đang nằm trên USB.** Dòng đầu tiên của menu GRUB là
`Build <giờ build theo giờ máy build, ví dụ 2026-09-23 18:34 +07> | <image-id>`, do
`build_boot_image.sh` điền vào lúc build. ISO thêm dòng thứ hai
`ISO <giờ đóng ISO> | nvidia-driver-580-open prebuilt`. Hai dòng này chỉ để xem, chọn vào thì chỉ nạp
lại menu. Mục mặc định được chọn theo `--id fault-attribution` chứ không theo vị trí, nên các dòng
thông tin không đổi mục tự boot. `/etc/cpu-dit/build-info` và manifest cũng ghi `built_local`.

## Hai loại module: open và proprietary (2026-10-05)

Module **open** của NVIDIA cần bộ xử lý GSP trên GPU, nên chỉ chạy từ đời Turing (GTX 16xx, RTX 20xx)
trở lên. Trên GTX 1050 Ti thật, kernel báo GPU "does not include the required GPU System Processor
(GSP)" và không nhận card. Đây là lý do GTX 9xx/10xx không chạy được khi ISO chỉ có
`nvidia-driver-580-open`. Ngược lại, Blackwell (RTX 50xx) **chỉ** chạy được với module open. Bản 580
là bản cuối cùng còn hỗ trợ Maxwell, Pascal và Volta trong module proprietary.

| Đời GPU | Ví dụ | open | proprietary |
|---|---|---|---|
| Maxwell | GTX 750 Ti, 960, 980 | không | có |
| Pascal | GTX 1050 Ti, 1060, 1080 | không | có |
| Volta | Titan V | không | có |
| Turing, Ampere, Ada | GTX 16xx, RTX 20xx/30xx/40xx | có | có |
| Blackwell | RTX 50xx | có | không |

Lớp driver nay chứa **cả hai bộ module cùng phiên bản** (580.178.04):

1. `tools/build_nvidia_driver_package.sh` tải thêm `nvidia-dkms-580` và `nvidia-kernel-source-580`
   đúng phiên bản của bản open vào `pkgs-proprietary/`. Hai gói này xung đột với bản open nên không
   nằm chung tập phụ thuộc; phần phụ thuộc còn lại đã có sẵn trong đó.
2. `tools/build_nvidia_driver_layer.sh` build bản open như cũ và cất vào
   `/usr/lib/dit/nvidia-modules/open/<kernel>/`. Sau đó nó gỡ hai gói open, cài và build DKMS bản
   proprietary, cất vào `.../proprietary/<kernel>/`, rồi trả bản open về `updates/dkms` làm mặc định.
   Build kiểm license của từng `nvidia.ko` (open: `Dual MIT/GPL`, proprietary: `NVIDIA`). Manifest
   của lớp ghi `kernel_module_flavors`.
3. Khi boot, `driver-probe.sh` gọi `select_nvidia_flavor` trong `nvidia-flavor.sh` trước khi nạp
   phần còn lại của driver. Chương trình đếm GPU NVIDIA trên PCI, nạp bản open và đếm số GPU nó
   nhận. Nếu còn GPU chưa nhận, nó gỡ module, chép bản proprietary vào `updates/dkms`, chạy `depmod`
   và nạp lại. Bên nào nhận được nhiều GPU hơn thì giữ; hòa thì giữ open. Ví dụ máy có cả card
   Pascal lẫn Blackwell thì quay lại open và card Pascal không được hỗ trợ. Không cần bảng mã thiết
   bị. Nếu không gỡ được module (console đang dùng) thì giữ open và ghi chú. Mỗi bước được ghi vào
   `driver-probe.txt` (`nvidia_flavor ... drives N of M NVIDIA GPU(s)`), lựa chọn cuối ghi vào
   `/run/cpu-dit-nvidia-flavor`.
4. `tools/test_nvidia_flavor.sh` (trong `make test`) kiểm logic này trên sysfs và `modprobe` giả lập:
   Turing, Blackwell, Pascal, Maxwell, Pascal + Turing, Pascal + Blackwell, thiếu bản proprietary,
   module bị giữ, không có GPU.

## Cách dùng

```sh
make image                                            # build image generic (không đổi)
make nvidia-driver-package NVIDIA_DRIVER_PACKAGE=nvidia-driver-580-open   # cần mạng + sudo
make nvidia-driver-burn-usb                           # burn generic image + gói NVIDIA lên cùng USB
```

Driver dựng sẵn, dạng ISO (khuyến nghị, không cài lúc boot):

```sh
make iso            # test + build image + lớp driver (nếu cũ) + ISO; cần mạng + sudo
make iso-only       # ISO từ image hiện có; chỉ build lại lớp driver nếu nó không khớp image
# driver khác: make iso NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open (mặc định 580-open)
```

Ghi `dist/dit-01-noble-amd64-nvidia.iso` ra USB ở chế độ **raw**: Rufus chọn **DD mode**
(không dùng ISO mode, vì ISO mode chép file sang FAT và mất phân vùng DIT-RESULTS), hoặc `dd`/Etcher.

Sau khi boot USB đó trên máy có GPU NVIDIA (chọn menu 3 — GPU-VRAM, như bình thường), rút USB ra
gắn vào máy khác (hoặc mount lại) và xem hai file trong thư mục **`boot/`** ở gốc phân vùng
DIT-RESULTS — **không phải** `runs/<session>/` (thư mục đó chỉ có report của chính GPU-VRAM, không
có hai log này, xem lý do ở mục trên):

```sh
cat /path/to/DIT-RESULTS/boot/nvidia-driver-install.txt   # cài có chạy, có thành công không
cat /path/to/DIT-RESULTS/boot/driver-probe.txt            # driver nào thật sự đang bind
```

## Đã kiểm

Trong sandbox phát triển (có mạng + sudo + `debootstrap`, dùng lại rootfs generic đã build sẵn ở
`build/boot-image/rootfs-noble-amd64`, kernel `7.0.0-30-generic`), không phải phần cứng NVIDIA thật:

- `bash -n` toàn bộ script mới/sửa; `tools/test_boot_policy.sh` mở rộng kiểm: thứ tự gọi
  (`nvidia-driver-install.sh` trước `driver-probe.sh`), nội dung từng bước (kiểm PCI vendor, kiểm
  đã chạy chưa, hai lớp sha256, gỡ nouveau, nhánh `nvidia_runtime` trong `driver-probe.sh`), cờ
  CLI của `burn_usb.sh`, nội dung của `build_nvidia_driver_package.sh` (kernel đúng rootfs,
  `--download-only`, `build-essential`, `dkms`, dọn cache).
- `make test`/`tools/test_unified_project.py` chạy lại PASS không hồi quy.
- **Chạy thật `make nvidia-driver-package NVIDIA_DRIVER_PACKAGE=nvidia-driver-580-open`**: tải qua
  apt 257 gói (driver + `nvidia-dkms-580-open` + `dkms` + `build-essential` +
  `linux-headers-7.0.0-30-generic` + toàn bộ closure, gồm cả Xorg vì metapackage NVIDIA phụ thuộc
  nó), đóng gói thành `dit-nvidia-driver-package.tar.gz` **445 MB**, `MANIFEST.sha256` của cả 261
  file (257 .deb + `install.sh` + `blacklist-nouveau.conf` + `manifest.json` +
  `driver-package-name.txt`) verify OK.
- **Chạy thật `install.sh` trong chroot** (copy sạch rootfs generic, mount `/dev /dev/pts /proc
  /sys`, `policy-rc.d` chặn service khởi động như `build_boot_image.sh` vẫn làm, giải nén gói,
  chạy `install.sh` không sửa gì): **phát hiện và sửa một lỗi thật** — một lần `dpkg -i pkgs/*.deb`
  duy nhất KHÔNG đủ, vì dpkg xử lý glob theo thứ tự alphabet và chỉ kiểm `Pre-Depends` với những gì
  **đã configure xong**, nên ví dụ `dkms` (Pre-Depends: `lsb-release`, đứng sau `dkms` theo
  alphabet) unpack thất bại ngay ở lượt đầu. Sửa bằng cách lặp lại `dpkg -i pkgs/*.deb` tối đa 6
  lần (hội tụ thật ở lượt thứ 3 khi thử tay). Sau khi sửa: chạy lại **từ rootfs sạch, không can
  thiệp tay**, `install.sh` PASS `exit=0` sau đúng một lần gọi, mọi gói `installed` (kể cả
  `nvidia-driver-580-open`, `dkms`, `python3`), và **DKMS build thật ra 5 module kernel**:
  `nvidia.ko`, `nvidia-drm.ko`, `nvidia-modeset.ko`, `nvidia-uvm.ko`, `nvidia-peermem.ko` dưới
  `/lib/modules/7.0.0-30-generic/updates/dkms/`, cộng `blacklist nouveau` được ghi vào
  `/etc/modprobe.d/dit-blacklist-nouveau.conf`. Đây là phần rủi ro nhất của thiết kế (cài offline
  không qua apt repo) và nay đã có bằng chứng thật, không chỉ suy luận.

## Chưa làm / cần phần cứng thật để xác nhận

- **Đã boot thật một lần trên RTX 5060** (2026-09-22) — lộ ra race `nouveau` (mục trên), đã sửa
  bằng `module_blacklist=nouveau` ở grub.cfg. **Chưa build lại image + burn + boot lại lần hai để
  xác nhận bản sửa** (image cần rebuild vì grub.cfg nằm trong ảnh; USB đang burn dở ở lượt trước đó
  dùng ảnh cũ). Ngoài driver có bind đúng không, còn cần xem GPU-VRAM có chạy được CUDA sau đó
  (`nvidia-smi` + `dit-cli`/`gpu-vram-dit` báo backend CUDA, không phải Vulkan/driver mở).
- Dung lượng gói đo được thật là 445 MB, vừa trong 1024 MiB mặc định của phân vùng DIT-RESULTS
  (`RESULT_MIB`) với nhiều chỗ trống cho kết quả test; driver bản khác/nhiều gói phụ thuộc hơn có
  thể lớn hơn — nếu không đủ chỗ, `--nvidia-driver-package` của `burn_usb.sh` báo lỗi copy rõ ràng,
  không âm thầm cắt bớt.
- Secure Boot, nhiều bản driver/kernel khác nhau, và multi-GPU chưa được xét riêng cho đường này.
