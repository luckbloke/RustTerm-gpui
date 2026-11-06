# NyaTerm IronRDP snapshot

- Upstream: `https://github.com/Devolutions/IronRDP`
- Commit: `b149f500b85124c513646494335fb6cee525d897`
- Snapshot date: `2026-09-22`

The workspace is kept as one coherent source snapshot. NyaTerm-specific changes must be
documented here and kept narrowly scoped so a future upstream refresh can be rebased as a
single unit.

## Local changes

- `ironrdp-client` accepts an application-provided direct transport stream so NyaTerm proxy and
  jump-host connections do not bypass the shared network stack.
- `ironrdp-client` accepts an asynchronous post-handshake certificate verifier. This keeps
  NyaTerm's known-host prompt and fingerprint policy authoritative with the Windows native-TLS
  backend, whose upstream validation callback is intentionally unavailable.
- `ironrdp-rdpdr-native` accepts an absolute local directory beneath a drive (`D:\dir`) as the
  redirected root, instead of requiring a whole volume (`D:\`). The Windows path conversion is
  shared with the existing volume-root path: it re-emits the `<letter>:\` prefix, collapses
  repeated separators, and still rejects drive-relative, root-relative, UNC, namespace, embedded
  NUL, and `.`/`..` forms. The normalized path keeps a trailing backslash so the root opens as a
  directory. Covered by unit tests in `windows/handles.rs`.
- `ironrdp-rdpdr` logs `[RDPDR] server rejected device announcement: ...` when the server refuses
  a redirected device. Upstream only records rejected IDs, which leaves a missing `\\tsclient`
  share undiagnosable from the client log.
- `ironrdp-rdpdr` serves filesystem IRPs that race against the announcement response instead of
  dropping them. Real Windows servers deliver the first create before
  `DR_CORE_DEVICE_ANNOUNCE_RSP`; leaving that IRP unanswered makes the server wait ~30 s and retry,
  and a later rejection removes the device and cancels its deferred operations. IRPs for unknown or
  rejected devices are still ignored. Same change as upstream #2191, rebased onto this snapshot.
- `ironrdp-rdpdr-native` caps a single read response at 16 KiB of data. The server requests
  `0x8000`-byte reads; answering with the full `0x8000`-byte-plus-header completion made Windows
  silently kill the redirected device after the first large read, so every later open and copy
  reported "0x8007048F 设备没有连接" while earlier reads kept succeeding. A short read is a legal
  partial completion and rdpdr.sys chains reads for the remainder.
- `ironrdp-rdpdr` announces filesystem device names as 8-bit null-terminated characters instead of
  UTF-16. Windows reads the `DeviceData` field as 8-bit characters, so a UTF-16 name surfaced only
  as its first letter (`\\tsclient\t` for `trd rw`; Windows 11 build 26300, Windows Server 2022 and
  2025 behave this way). Non-ASCII and `PreferredDosName`-forbidden characters become `_`, a
  trailing colon is kept. Same change as upstream #2342, rebased onto this snapshot.
- `ironrdp-rdpdr` and `ironrdp-rdpdr-native` print every announced device name, every filesystem
  IRP, and every refused IRP on stderr when the environment variable `NYATERM_RDPDR_TRACE` is set.
  The consuming application installs no `tracing` subscriber, so upstream's `debug!`/`trace!`
  records would never be visible while debugging a "无法访问 \\tsclient\..." report.
- Both crates also append the always-on diagnostics (announced names, accepted/rejected
  announcements, dropped IRPs, and every refused read/write/create/query IRP) to the file named by
  `NYATERM_RDPDR_LOG`, because stderr is invisible for windowed (`windows_subsystem = "windows"`)
  launches, which is how end users run the app.
- `ironrdp-rdpdr-native` drops create-option bits it does not model instead of refusing the whole
  create IRP with `STATUS_NOT_SUPPORTED`. Explorer sends cache/capacity hints (for example
  `FILE_NO_INTERMEDIATE_BUFFERING`) when opening a redirected volume; MSTSC and FreeRDP ignore
  unknown bits, and so refusing them makes a healthy share unopenable on the remote side.
  `FILE_OPEN_BY_FILE_ID` is still refused because it cannot be resolved inside the redirected root.
- `ironrdp-rdpdr-native` no longer sets `OBJ_DONT_REPARSE` when opening the redirected root by its
  absolute NT path. The `\??\<letter>:` prefix of any drive path is a symbolic link by design, so
  that flag made every root open fail with `STATUS_REPARSE_POINT_ENCOUNTERED` (0xC000050B), the
  server kept retrying the create IRP forever, and File Explorer on the remote side reported
  "无法访问 \\tsclient\...". The flag still applies to component names opened relative to the
  redirected root handle, where it keeps server-controlled paths from escaping through reparse
  points. Covered by the `open_object_attributes` unit tests and the real-I/O root tests.


