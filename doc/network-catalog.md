# Network capability catalog

`data/network-cards.toml` is a community-editable catalog embedded in the MDMA
binary. No network lookup, extra privileges or external database is required.
Update the file, run `cargo test --locked --bin mdma node::network`, and rebuild.
Existing daemons keep their embedded catalog until updated.

## Three independent layers

* `hardware_identity`: guest/OS-visible bus and device IDs.
* `hardware_capabilities`: a source-backed card/family ceiling, not throughput.
* `driver_capabilities`: an independently matched OS/driver ceiling, not a
  guarantee that the radio, firmware, tunables or antenna support every mode.
* `link_speed_bps`: the observed negotiated link, never filled from the catalog.

Unknown profiles are null. Empty standards mean unverified, not unsupported.
`max_phy_bps` is a theoretical PHY ceiling in bits/second; omission means unknown.
We do not derive a Wi-Fi rate from "ax", sum streams, or use a theoretical
ceiling as a bandwidth measurement. Driver limits are not a fallback for a
missing negotiated speed. Source references and qualifications travel with
each matched profile over the resource RPC.

Example: a Realtek family profile says 1 Gbps while its live link reports
100 Mbps. FreeBSD 14.2's documented iwlwifi compatibility ceiling is a/b/g
(54 Mbps PHY), whereas 14.3 adds conditional n/ac support for 22000 and newer
generations. Neither implies that every supported card or configuration
achieves those rates.

## Adding hardware

Every entry needs a unique `id`, `bus`, four-digit lowercase `vendor`/`device`,
`model`, `sources`, and `notes`. Optional four-digit `subsystem_vendor` and
`subsystem_device`, and two-digit `revision`, narrow the match.
Optional `standards` and positive `max_phy_bps` need evidence.
Bus namespaces are `pci`, `usb`, and `apple_airport`: Apple's AirPort Card Type
IDs must **not** be treated as PCI IDs.

More-specific identity entries override generic family entries. Equal-specificity
overlap is ambiguous and yields no profile, not file-order precedence.
Missing subsystem/revision data never matches an entry requiring it. Unknown
fields, malformed IDs, zero rates, duplicate entry IDs, unsupported schema
versions and missing sources fail validation.

Prefer vendor datasheets or revision-pinned driver documentation. State whether
a model names a controller family, actual radio module, or retail adapter.
Supported modes observed on one machine do not establish bands, antenna count,
channel widths or a numeric maximum for every card with a related identifier.

## Adding OS/driver limits

Rules require exact `os` and `driver` identifiers, numeric `os_min` inclusive
and `os_before` exclusive bounds, `sources`, `notes`, and a unique `id`.
Optional `bus`/`vendor`/`device` must be supplied together. `os_name` narrows to
the reported OS name; Linux rules require it so distro versions are not mixed.
Kernel/driver versions and firmware may differ within a distro: don't add broad
numeric limits without evidence covering the range.

`kernel_revision` optionally pins a hexadecimal kernel commit (at least 12
characters), matched against the detected kernel-build identifier. CURRENT and
STABLE builds **require** a matching revision-pinned rule. A rule verified on
one CURRENT checkout is never assumed to cover the whole development branch.
Bounds are numeric, so 14.10 is not confused with 14.1. Missing/unparseable OS
versions or equal-specificity overlapping rules remain unknown.

## Detection and virtualization

Linux reads PCI IDs from the selected NIC's sysfs device; macOS extracts only
interface names and Card Type IDs from system_profiler and caches them;
FreeBSD parses exact pciconf device records. FreeBSD wlan parent resolution is
best-effort: unresolved parents remain unknown rather than selecting an
arbitrary radio. Other buses/unsupported platforms can remain unidentified even
though the schema supports contributor entries.

Identified guests and software virtual interfaces do not receive physical
hardware/driver capability profiles. A guest-visible emulated PCI ID is not
evidence of the host's adapter. Even passthrough is conservatively unknown when
guest evidence is present. We do not export SSIDs, access-point scans, MAC
addresses or machine serials from the identity collector.

## Observed hardware

Catalog seed evidence:

| Platform | Evidence | Identity |
|---|---|---|
| macOS | system_profiler SPAirPortDataType | AirPort 14e4:4378; PHY a/b/g/n/ac/ax |
| macOS | system_profiler SPAirPortDataType | AirPort 14e4:4387; PHY a/b/g/n/ac/ax |
| Linux | sysfs + lspci -nn | PCI 8086:51f0; Intel Alder Lake-P PCH CNVi Wi-Fi; subsystem 8086:0094 |
| FreeBSD | pciconf -l | PCI 10ec:8168; subsystem 10ec:0123; revision 15 |

The FreeBSD CURRENT rule requires source/kernel revision `7c94a196f533`
(16.0-CURRENT). Its pinned `share/man/man4/re.4`
documents the RTL8168/8111 family's 10/100/1000 Mbps capabilities. The catalog
does not apply that rule to other CURRENT builds. The release-specific iwlwifi
rules reference the corresponding FreeBSD 14.2 and 14.3 manuals, not CURRENT.
