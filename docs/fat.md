<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# FAT32 block volumes

The Native `fs-fat` worker (default path `/boot/svc/fs-fat` after the root switch)
mounts an exclusively owned logical block volume,
not a copy of its contents in ramfs. `hyper_fs_fat::block::BlockDevice` describes
512-byte sector transfers and a durable flush. The kernel retains generic VFS
and cache coordination; FAT media parsing runs in the worker. See
[filesystem services](https://hyper-proj.github.io/lib-fs-service/docs/filesystem-services.html) for process startup, authority,
transport and failure semantics.

FAT parsing and mutations use the MIT-licensed `rafalh/rust-fatfs` revision
`2aefc2a027ce94ed0671752814dac203f0450e11`, vendored in
`third_party/rust-fatfs`. Its provenance document records the local correctness
and bounded-stack fixes; the original MIT copyright and license are retained.
Its no-std, no-alloc configuration provides long filenames. HypeR keeps the
library behind `hyper_fs_fat::FatVolume`; upstream files and directories do not
escape the adapter.

## Media admission

Before upstream parsing, HypeR validates the BPB geometry and arithmetic bounds,
FAT successor ranges, directory references, reachable chain lengths, cycles and
crosslinks. A temporary two-bit-per-cluster allocation records ownership; it is
released after mount. The worker
allocates this bounded scratch from its resource domain. Kernel namespace
bindings, node leases and retained paths are separately charged to the mount
sponsor.
File contents are not cached or copied by this scan.
The directory graph admits at most 65,536 directories. An operation also has a
finite I/O/seek budget so a corrupt chain cannot cause an unbounded traversal.

These checks require exclusive ownership for the entire mounted lifetime. The
I/O VM must not mount or otherwise modify the filesystem. A backend that changes
sector contents behind HypeR's cache violates the block volume contract. This
adapter does not claim to defend against a malicious trusted I/O VM.

FAT has no journal. A power failure during metadata changes can require an
external filesystem checker. If mkdir runs out of parent-directory space before
publishing the short entry, its unlinked child cluster is returned to the FAT.
This rollback is not attempted after uncertain device I/O: metadata may already
have reached the device. A transport error poisons the mounted instance;
subsequent accesses fail instead of continuing with potentially inconsistent
metadata. Device errors may have committed part of an operation.

## Synchronization and lifetime

Writes complete before the volume operation returns. Within an operation, eight
128 KiB heap windows coalesce copied data writes and repeated FAT sector
updates. Aligned file payload from the kernel-owned read-only source window
bypasses these buffers: the adapter collects source-range descriptors instead
of copying the payload. Adjacent disk and source offsets coalesce; a batch
submits at 64 KiB, the range limit, or the operation boundary. Pending metadata
is submitted before source references, and pending references before copied
metadata or a conflicting read. Dirty-sector bitmaps retain only modified sectors; contiguous dirty
runs become block requests. Each worker owns one logical block queue with sixteen
completion slots, a 2 MiB retained-payload limit and at most four device commands
in flight. These are limits rather than eagerly allocated payload storage.
One submission contains at most sixteen disjoint ranges totaling 512 KiB.
Windows with a contiguous 64 KiB dirty run submit immediately so device work
can overlap subsequent FAT traversal even when a 128 KiB file write straddles
physical window boundaries. Eviction and upstream stream flushes submit partial
windows; smaller sparse metadata writes remain coalesced until those boundaries.

Copied submission retains a private kernel payload before returning, so the
worker can reuse its own metadata buffers while earlier I/O remains in flight.
Source-window submission copies descriptors only and retains a kernel lease on
the active operation's bytes. The readonly source window cannot be reused until
all its accepted work completes and the filesystem request finishes. Admission
backpressure reaps any completed local batch before retrying, instead of waiting
for all older batches. If this worker has no outstanding batches, it waits for
the requested byte count and one free slot in the shared device budget; another
queue's temporary pressure does not poison the mount. Later overlapping reads
and writes use the same queue and observe device submission dependencies.
Disjoint work can complete out of order.

Every volume operation joins all accepted writes before replying, including
after a read, allocation-budget or other operation failure. Completion cleanup
does not pass through the poisoned-device admission gate. It publishes any
previously accepted pending range batch and joins submitted work; it does not
start another filesystem mutation. A recoverable error such as no space first submits completed
metadata rollback. An uncertain device error poisons the volume without retrying
a partial batch. The completion boundary preserves synchronous file-write
semantics; this is not a cross-operation dirty file cache.

Buffered sectors are the authoritative read view until submitted. Four 4 KiB
read-through windows batch nearby metadata fields and keep directory and FAT
lookups from displacing each other on every read; bulk file data bypasses them.
Writes update overlapping read windows, and failed writes invalidate them.
Fixed media buffers, including the 1 MiB write buffer, reside in the
worker and consume its process memory quota. Queue payloads are separately
bounded and charged by the kernel. Ordinary writes do not claim stable-storage durability. `sync`
explicitly commits upstream FSInfo and directory metadata, drains the adapter,
issues the block device's durable flush, and retains the mounted metadata view.

The filesystem also retains sixteen coalesced FAT-chain seek checkpoints. These
avoid traversing a file from its first cluster on every positioned write.
Checkpoints belong to the mounted filesystem, so reopened files share them
without retaining upstream file editors. Appending preserves existing positions;
freeing or truncating any chain invalidates all checkpoints before mutation,
even when mutation subsequently fails. Fragmentation falls back to walking from
the nearest retained checkpoint. This bounded inline cache allocates no memory
per seek and never changes FAT allocation or error handling.

File reads retain bounded allocation maps for four paths, with at most 128
coalesced disk extents per path. Once mapped, reads locate the requested offset
directly and combine adjacent data sectors instead of reopening and walking the
FAT chain for every transfer. Each map also owns four 4 KiB read-through windows
for partial-sector file reads, avoiding repeated device requests while parsing
image headers. Aligned bulk reads bypass these windows. All of this fixed
storage resides in the worker resource domain. The remote bulk-read operation
returns up to 64 byte-range extents per prefix instead of copying file bytes
through the worker. The kernel executes the plan directly into private cache or
request pages via the I/O VM exchange pool. Highly fragmented files walk the FAT
chain to produce bounded prefixes without allocating an unbounded extent table;
ordinary buffered reads remain available as a fallback.
All potentially mutating operations invalidate maps and their windows before touching media,
including operations that subsequently fail. The sole worker serializes map
construction, use and invalidation. An initial map build takes the size from
the open file's directory entry and traverses its chain once; it does not first
seek through the chain to EOF. This cost is amortized across subsequent reads
until mutation or eviction.
An exclusive filesystem borrow ensures all temporary file editors have been
dropped before synchronization. It does not reparse the boot sector or repeat
the admission scan. A failed synchronization closes the mounted view.

Upstream destructors attempt I/O. Device access is therefore gated to explicit
operations, with failures latched and returned to the caller. Dropping the
HypeR volume closes this gate before dropping upstream state; no destructor
can submit I/O. Upstream's frequent stream flushes only submit buffered transfers;
they do not turn every file read or stat into a physical cache barrier.
Graceful provider handoff joins completions before returning the original block
capability and moving the same queue and source window with the live transport.
The old media adapter drops its queue alias; the replacement retains the
transport's original queue authority. An abnormal queue close does not imply
that physical DMA has stopped; the kernel retains that ownership until retirement.

A read-only block volume rejects mutations before changing cached metadata.
Read, stat, synchronization and teardown issue no writes or device flushes;
a rejected write does not poison subsequent reads.

Directory traversal is iterative. A borrowed-entry visitor owns the temporary
FAT long-name buffer inside its traversal frame. Metadata lookup copies into
the volume's caller-owned output buffer; directory and file lookup return only
a small stream or cursor. The borrowed entry cannot escape the visitor. This
keeps owned long-name return values out of the outer VFS call chain without
allocating on each I/O operation. The public upstream
iterator still uses the same parser and retains its end/error behavior.
Private mutation cursors retain only the located short-entry metadata, byte
positions and filesystem owner; rename, remove and creation do not carry an
unused long-name buffer into subsequent disk operations.

`FatVolume::read_directory(path, cursor, output)` returns a batch of at most
4 KiB containing names, attributes, an EOF flag and per-entry continuation
checkpoints. The initial zero cursor resolves the directory path; subsequent
requests restore its first/current cluster and record offset directly. They do
not repeat parent lookup or walk the cluster-chain prefix. Long-name parsing
resumes only at complete directory-entry boundaries. Independent scans retain
separate checkpoints in the kernel, without an iterator table or whole-directory
snapshot in the worker. The older indexed `entry(path, index)` API is removed.

These physical checkpoints are trusted kernel state, not application cookies.
The kernel binds each one to its mounted directory and invalidates affected
streams before directory changes or ancestor moves. The adapter additionally
checks the cursor format, path fingerprint, cluster bounds and record alignment;
those checks are not a substitute for the kernel's authority and mutation
fence. Changes to unrelated directories do not invalidate a scan. Kernel
directory and attribute caches share the file-cache pressure and reclamation
mechanism; discarding a cached batch leaves the stream's small continuation
state available for another bounded worker request.

Parent resolution and directory scanning use separate stack frames. Metadata
inspection also completes before mutation starts, so its filename scratch is
not retained across the later blocking operation. The FAT engine uses the userspace worker stack; compiler frame reports still
check bounded local frames, while QEMU kernel stack watermarks cover the VFS and
IPC path; a local frame limit alone is not
a whole-call-chain bound. Watermarks measure modified bytes, not untouched stack
reservations. Audit builds therefore complement static review and are not used
for performance measurements.

## VFS semantics

Regular-file read caching belongs to the common VFS layer, described in
[VFS file-data cache](https://github.com/HypeR-Proj/kernel/blob/main/docs/design/vfs.md#file-data-cache). The generic remote adapter supplies positioned reads and metadata over RPC;
its local endpoint-health check detects a closed or failed volume without RPC.
The shared file record owns the common content gate and revision; all writes,
resizes and truncating opens pass through that gate before entering FAT.
Alternate names therefore share cached contents and mutation ordering. Cached
pages retain that content record, independently of active node leases, and do
not prevent unlink.
These clean pages do not change the synchronous write or explicit sync contract.
When the last live lease closes, resident or reader-pinned pages can preserve
the record. Reopening the canonical name or an alias then reuses its identity,
revision and known length. After both active leases and cached pages disappear,
the next open gets a fresh, never-reused incarnation. Sharing is therefore
opportunistic across close/reopen and across processes, not a permanent inode
table or a promise to retain pages under pressure.

Weak namespace bindings retain charged path storage until a later lookup or
bounded scheduled cleanup removes them. Records and paths remain charged to
the mount sponsor. Each binding has its own fallible allocation, so removing it
releases its storage and charge without retaining a historical table capacity.
Its charge also covers the full node and content-record allocations retained
by weak references, conservatively overlapping their charges while live.
Balanced indices locate bindings by ID and canonical path, comparing full
paths even when their placement hashes collide. A numeric successor cursor
allows bounded housekeeping without rescanning a prefix; index removal
allocates nothing, and teardown is iterative. A quota failure during
unpublished preparation requests one finite
cache sweep for the denying resource domain, including descendant sponsors.
The worker drops matching clean pages outside the cache lock and tries idle
namespace cleanup without waiting for a volume mutex. The requester then
prunes its own expired bindings and retries preparation while new cache
admissions remain paused. Active leases, in-flight readers and concurrent quota
users can still prevent admission; namespace mutations and handle callbacks
are never replayed.

Long filenames and their 8.3 aliases resolve to the same canonical stored name,
so alternate spellings cannot create separate live node or lock identities.
Exclusive creation rejects an already existing entry through either spelling.
Node leases have stable in-memory IDs distinct from reusable FAT directory
slots. Live leases prevent unlink. Successful rename updates active and cached
idle descendant paths under the volume lock. Creation prepares its owners and
binding allocation before media mutation and publishes the binding only after
success. Unlink retires the name binding before a same-name creation can resolve
it, so retained old pages cannot identify the replacement. Mount pins additionally
prevent moving a mounted subtree. A newly created same-name file therefore
cannot become visible through a previous file's handle.

FAT cannot represent hard links, symbolic links, or general Unix permission
bits. These operations report unsupported; modes are synthesized from the FAT
read-only attribute (`0777`, or `0555` when read-only). A requested creation mode
such as `0600` does not provide persistent Unix permission isolation on FAT;
Native handle rights still constrain each granted capability independently.
File creation and updates use the Native realtime clock, with UTC
as the FAT time convention. File access times have day precision, modification
times have two-second precision, and creation times have ten-millisecond
precision. Explicit timestamps outside 1980–2107 are rejected; invalid dates
read from media are reported as unavailable. Directory timestamp changes are
unsupported. Rename currently reports an existing destination rather than
replacing it. Ramfs retains its existing semantics.
