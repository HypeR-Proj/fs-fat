<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# fs-fat

A HypeR component. See [HypeR](https://github.com/HypeR-Proj/HypeR) for
workspace setup and the installed SDK build boundary.

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

## Workspace tests

Use the [HypeR workspace](https://github.com/HypeR-Proj/HypeR) to resolve dependencies:

```sh
python3 project.py test fs-fat
```

Paths in the documentation are relative to the repo-managed workspace root.
Edit this repository and prepare the workspace again before building.
