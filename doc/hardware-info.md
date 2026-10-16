# Hardware inventory

`mdma info` is a read-only Linux, FreeBSD and macOS inventory command. It does not load a model,
initialize an inference backend, run benchmarks, change clocks or elevate privileges.
It builds with the default CPU feature set.

```sh
cargo build --release --bin mdma
target/release/mdma info
target/release/mdma info --json
```

## Report contents

- **RAM:** per-populated-slot capacity, DDR type, form factor, manufacturer, part
  number, rated speed and configured speed. Installed module capacity is separate
  from OS-usable RAM and currently available RAM.
- **CPU:** manufacturer, model, architecture, sockets, physical cores, logical
  threads, observed and nominal frequencies, reported min/max frequency, and caches.
  Hybrid CPUs do not assume every core has the same number of threads. The
  instruction sets mdma's kernels can use (`avx2`, `avx512vnni`, `dotprod`,
  `i8mm`, …) are detected at run time with the kernel variant they select
  (`instruction_sets`, `kernel_level`).
- **GPU:** PCI display/3D devices (including headless devices) and macOS built-in GPUs. Chip and board
  manufacturers, driver, VRAM capacity/manufacturer and clocks are included where
  exposed by the installed driver.
- **Ports:** PCI domain/bus/device/function address, firmware/ACPI physical slot
  when available, the upstream PCI bridge path, and the device's current and
  maximum PCIe link generation/width. DRM HDMI/DisplayPort connection status is
  listed separately; display connections are not the GPU's PCIe connection.
  Apple unified memory is labeled explicitly: it is not counted as dedicated VRAM,
  and a built-in Apple GPU is not assigned a fabricated PCI address.
- **Accelerators:** Coral Edge TPU PCIe/M.2 cards (`1ac1:089a`). On Linux: driver,
  `/dev/apex_N` node, status, temperature and PCIe link from sysfs, plus Coral USB
  Accelerators (`1a6e:089a` before the runtime firmware is uploaded, `18d1:9302`
  after). On FreeBSD: the `apex` driver attachment and `/dev/apexN` from `pciconf`,
  temperature from `dev.apex.N.temperature`, and the PCIe link when `/dev/pci` is
  readable. Detection only: the device is not opened, and mdma has no Coral backend
  yet. `mdma doctor` lists detected accelerators after the backend checks.

The JSON report has `schema_version: 1`. Missing data is `null`, not zero; warnings
are part of the report. Serial numbers and UUIDs are deliberately not collected.
An explicitly supplied unreadable `--dmi-file` is an error rather than a silent fallback.
Collectors are selected at compile time for these three operating systems; there
is no unsupported-OS runtime fallback.

## Sources and permissions

### Linux

`lscpu` (util-linux) supplies CPU labels and caches; `/proc/cpuinfo` provides a
fallback and observed frequency. CPU sysfs topology supplies physical core counts.
`/proc/meminfo` supplies OS RAM totals.

`dmidecode --type 17,9` decodes firmware RAM modules and slot names. Normal-user
SMBIOS access is often denied. The command still returns the rest of the report
and explains the limitation. It never invokes `sudo`. Firmware may report unknown
or inaccurate module information even with sufficient permissions.

To collect only the firmware information with privileges, then run the inventory
as your normal user:

```sh
# Run this yourself only if your local permissions allow it.
sudo dmidecode --type 17,9 > memory-and-slots.txt
target/release/mdma info --dmi-file memory-and-slots.txt
target/release/mdma info --json --dmi-file memory-and-slots.txt
```

`lspci` (pciutils) supplies GPU product and board names. PCI/DRM sysfs supplies IDs,
drivers, link information and display connectors. AMD sysfs exposes VRAM capacity,
memory manufacturer and DPM clocks; Intel DRM clock attributes are read when
present. NVIDIA VRAM/clocks use optional `nvidia-smi` queries. Missing utilities,
drivers or attributes produce unavailable fields rather than invented specs.
An integrated GPU without a dedicated VRAM counter is not assigned all system RAM.

### FreeBSD

`sysctl` supplies the CPU model, physical cores/thread counts, RAM totals and
frequency information. Current CPU frequency is the CPU 0 value; nominal frequency
and the cpufreq driver's reported limits are separate. Package/socket counts remain
unknown when the kernel does not expose them.

`pciconf -lvc` supplies PCI GPU identity and negotiated/maximum PCIe links. Reading
capabilities may require access to `/dev/pci`; on permission failure the collector
uses `pciconf -lv` for unprivileged identity and leaves link fields unavailable.
PCI selectors are converted from FreeBSD's decimal notation to standard hexadecimal
domain:bus:device.function addresses. BAR aperture sizes are never treated as VRAM.

`dmidecode` is optional for module brands, DDR types, speeds and slot names; a saved
`--dmi-file` works without it. FreeBSD's `hw.realmem` supplies reported physical RAM
when module information is unavailable. The available-RAM estimate uses free,
inactive and cached pages. `pciconf` alone does not expose VRAM capacity, GPU clocks
or display connectors; those remain unavailable rather than inferred.

### macOS

Native `system_profiler -json` supplies CPU/chip labels, RAM details and GPUs with
connected displays. `sysctl` supplies core counts, cache/frequency metadata and
`hw.memsize`; `vm_stat` supplies an available-RAM estimate from free and inactive
pages, using its reported page size. Nominal CPU frequency is not an observed clock.

Apple silicon reports unified system RAM, not a separate VRAM allocation. Intel
integrated GPU shared-memory allowances are also not presented as dedicated VRAM.
Discrete GPU VRAM is reported when `system_profiler` supplies it. Current clocks,
full PCIe routes, motherboard slot names and RAM speeds may not be exposed.
No Metal build feature is needed. `--dmi-file` can optionally replace RAM module
metadata with a saved firmware report; the command does not invoke `dmidecode` by
default on macOS.

## Units and limitations

- Capacities use bytes in JSON and GiB in text. Driver-reported VRAM may exclude
  reserved memory and therefore be smaller than the marketed capacity.
- RAM rates retain the firmware-decoder unit, normally **MT/s**. Legacy
  `dmidecode` output labeled **MHz** is preserved, not silently converted.
- CPU/GPU clock fields use **MHz**. Linux CPU current frequency is an average observed
  across the `/proc/cpuinfo` sample; FreeBSD reports CPU 0. Maxima are driver-reported/profile limits,
  not measured sustained clocks. VRAM clock is not its effective transfer rate.
- PCIe speed uses **GT/s**, not GB/s. Current links can downshift while idle;
  endpoint maximum does not prove the whole upstream path supports that rate.
- This reports hardware metadata, not memory bandwidth or inference performance.
  See [runtime tools](runtime-tools.md) for `doctor`, memory planning, copy
  benchmarks and explicit CPU/GPU layer offloading.

## Focused native validation

The development-only Cargo harness imports the actual inventory collector into
an info-only CLI without the inference dependency graph, so inventory can be checked on another
machine without a GPU SDK:

```sh
cargo test --manifest-path tests/hardware-cli/Cargo.toml -j 1 -- --test-threads=1
cargo run --manifest-path tests/hardware-cli/Cargo.toml -j 1 -- info --json
```

Platform-specific parser tests run on their native OS. The harness and its tests
are excluded from the published crate.
