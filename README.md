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

## Write progress and storage errors

A write reports the input prefix accepted by the filesystem. If allocation runs
out after a prefix, the reply is a short count; retry only the remaining suffix.
Append positions advance by that count, not by the original request length.
Zero filling before the requested offset is not input progress. A failed gap
extension may leave zero-filled bytes even when no input was accepted.

Accepted bytes are not a durability guarantee. A later buffered-write or device
completion failure does not erase a known count. The volume remains poisoned,
rejects further operations and reports failure on sync. All accepted asynchronous
requests are drained before their source window is released. If the failing
transfer itself has indeterminate effects, its bytes are not counted; an error
does not promise rollback or make blindly replaying an append safe.

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

## Synchronous mutation policy

The shared service defaults FAT to synchronous mutations. Write, create, remove,
rename, resize and timestamp changes synchronize filesystem metadata and complete
the device flush before returning. Independent block transfers can still overlap
inside that operation; there is no delayed dirty-page writer. Accepted write
prefixes are preserved if the final fence fails, and that error remains visible
through subsequent requests/sync. This does not make interrupted FAT metadata
updates transactional. The independent bootstrap worker follows the same policy.
