<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# fs-fat

Bounded FAT32 adapter and Native filesystem worker.

Adapts the vendored rust-fatfs implementation to bounded block access and the `lib-fs-
service` worker contract. The volume owns media, operation budgets, caches and coalesced
writes; VFS owns namespace identities and open-file leases.

See the [implementation entry](docs/fat.md) and [component declaration](component.json)
for the public interfaces, build dependencies and delivered artifacts.

## Boundaries

Filesystem operations require a sleepable mutex. Upstream objects stay inside the
adapter because destructors can perform I/O; failures poison the volume before unsafe
reuse. FAT is not a journaled filesystem, and accepted writes are not a durability
guarantee.

## Build and test

Use this checkout inside the [repo-managed workspace](https://github.com/HypeR-Proj/HypeR).
From this repository directory:

```sh
make
make test
```

An absolute `OUT=/path/to/out` selects a shared output directory. The module
entry delegates to build/system, which prepares the installed SDK and declared
dependencies. Native binaries require HypeR; host module checks cover the tests
selected by component.json. A standalone clone does not contain the workspace’s
other source repositories or Native SDK.
