# POSIX conformance: pjdfstest against an XVFS mount

Date: 2026-07-27
Reproduce: `./spikes/conformance/pjdfstest.sh <mounted-workspace>`
Closes, in part: the gap recorded in
[M2](m2-completion.md#pjdfstest-and-xfstests-were-not-run) and restated in
[M3](m3-completion.md), flagged "should close before M6.1".

PLAN.md M2.4 asks for a relevant subset of `pjdfstest` and xfstests. Both M2 and
M3 recorded that neither had been run and that `tests/compat.rs` covered a
hand-written subset instead. **pjdfstest now runs.** xfstests still does not; see
the last section.

## Method

The suite is run **twice by the same unprivileged user** — once against ext4 and
once against an XVFS mount — and only the difference is reported.

That is not tidiness. pjdfstest's README says "You must be root when running
these testcases", and root is unavailable here: not merely absent, but unhelpful,
because a root process cannot enter a uid-1000 FUSE mount without
`user_allow_other` in `/etc/fuse.conf`, which ADR 0003 treats as a privileged
host action. Run as an ordinary user, **76 of 238 test files fail on ext4**, for
reasons that have nothing to do with XVFS. Reporting XVFS's raw failure count
against POSIX would therefore be meaningless.

So ext4 is the oracle, the same way the raw tree is M2's oracle for the mount and
ripgrep is M4's for search: a suite that cannot be its own baseline is given one.

Two environment notes, both of which produced a plausible-looking wrong answer
before they were found:

- **`core.autocrlf=true`** is set globally here, so the first clone delivered the
  `.t` scripts with CRLF endings and all 238 failed with `": not found"` on their
  odd-numbered lines. That reads as a broken suite rather than a broken checkout.
  The M3 report records the same setting corrupting `git apply`. The script forces
  `core.autocrlf=false`.
- **autoconf/automake are not installed**, so `config.h` is hand-written rather
  than probed. pjdfstest is a single C file and every `HAVE_*` set is a
  POSIX.1-2008 call glibc has.

| | first run | after the fixes below |
| --- | ---: | ---: |
| test files run | 238 | 238 |
| clean on ext4 (the baseline) | 162 | 162 |
| clean on XVFS | 126 | **128** |
| **XVFS-only failures** | **37** | **35** |
| XVFS-only passes | 1 | 1 |

Two defects were found, root-caused and fixed in the same pass; a third was
root-caused and is a decision rather than a fix. See "What was fixed" below.

The single XVFS-only *pass* is `rename_22`, whose ext4 failure is a root-only
`mknod b`. Not root-caused; it is an artifact of the unprivileged control rather
than a claim that XVFS is more correct.

## The 37, triaged

This is the triage of the **first** run. Sections 3, 4 and 5 carry the fixes that
followed; the rest stand as recorded.

### 1. Object types XVFS does not implement — 5 files

`mkfifo_02`, `mkfifo_03`, `open_17`, `open_24`, `rmdir_01`

`mkfifo` and `mknod` return `EPERM`. Every failure in these files is a setup step
that needs a FIFO, a device node, or a UNIX domain socket — `rmdir_01` is
labelled "rmdir returns ENOTDIR" but fails only where it first creates a FIFO.

**Assessment: by design, and already stated.** DESIGN.md section 12 lists special
files as out of scope. Worth confirming `EPERM` is the errno the design intends,
since `EOPNOTSUPP` is the more usual answer for "this filesystem cannot".

### 2. Hard links — 1 file

`link_09`

`link` returns `EPERM`. A Git tree has no hard-link concept, so this is expected;
it is recorded because the design does not appear to say so explicitly.

### 3. Long-path errno — 18 files

`chmod_02` `chmod_03` `ftruncate_02` `ftruncate_03` `link_02` `link_03`
`mkdir_03` `open_02` `open_03` `rename_01` `rename_02` `rmdir_02` `rmdir_03`
`symlink_03` `truncate_02` `truncate_03` `unlink_02` `unlink_03`

The largest cluster, and the one that contains something bug-shaped.

The straightforward boundaries are **correct** and were checked directly against
ext4: a 255-byte component succeeds, 256 gives `ENAMETOOLONG`, and paths of
4 115–4 236 bytes give `ENAMETOOLONG` on XVFS and ext4 alike. `compat.rs`'s
existing `an_over_long_name_is_refused_by_the_kernel_before_it_reaches_the_daemon`
holds.

What the suite finds is narrower and worse:

- **`EIO` for some long-path shapes.** `open_03` assertion 1 and `link_03`
  assertion 1 build a deep path out of ~30 components of ~126 bytes and expect
  the create to *succeed*; XVFS returned `EIO`. From a FUSE filesystem `EIO` means
  the daemon returned an error the kernel could not classify — an internal
  failure surfacing as I/O error, not a POSIX answer. **Root-caused and fixed;
  see "What was fixed".** The boundary is now exact: an in-workspace path of
  4 096 bytes succeeds and 4 097 gives `ENAMETOOLONG`.
- **`ENOENT` where `ENAMETOOLONG` is expected**, for an over-long component in a
  path whose parent also does not exist (`chmod_02` assertion 5). Arguably
  defensible ordering, but it differs from ext4.

### 4. Directory timestamps are never updated — 2 files

`rmdir_00`, `symlink_00`

Both fail on `test_check $time -lt $mtime` and the `ctime` equivalent: **a
directory's mtime and ctime do not move when a child is created or removed.**
Confirmed directly — across a create and a delete, XVFS reported the same mtime
three times while ext4 incremented on each.

**Assessment: a real divergence, and the one with the widest blast radius.** Build
systems, watchers, and glob caches use directory mtime to detect that a
directory's contents changed. DESIGN.md section 12 documents base timestamps as
the sanitized snapshot time and an overlay floor, but that is about *values*; it
does not say directory mtime is inert under mutation.

**Fixed; see "What was fixed". Both files are now clean.**

### 5. `utimensat` semantics — 5 files

`utimensat_02` `utimensat_04` `utimensat_05` `utimensat_08` `utimensat_09`

`UTIME_OMIT` does not leave the time unchanged (a set atime came back as the
mount's snapshot time); atime and mtime cannot be set independently; subsecond
precision and post-2038 values do not round-trip.

**Assessment, after root-causing: one cause, and it is a scope decision.** All
five reduce to `atime` not being stored — `attr.rs` reports `atime: mtime` and
`setattr` ignores its `_atime` argument. The first reading of `utimensat/09`
("post-2038 values do not round-trip", 2³¹ read back as 2³²) was wrong: the test
sets atime to 2³¹ and mtime to 2³², and XVFS returns mtime for both. There is no
arithmetic bug. See "What is a decision, not a defect".

### 6. Mode fidelity — 1 file

`open_26`

Creating with mode `0000` yields `0644`. Together with `chmod_02`'s
`chmod 0620` → still `0644`, the picture is that the overlay models Git's two
modes — regular and executable — and **silently rounds everything else**, while
returning success.

**Assessment: by design in substance, wrong in reporting.** Git stores only 644
and 755, so the value cannot be preserved; a `chmod` that returns 0 and does not
change the mode is the "succeeds but did not do what you asked" shape this
project avoids elsewhere. Whether to refuse or to keep rounding is a decision,
not a bug fix — but it should be a stated one.

### 7. `nlink` semantics — 2 files

`rename_24`, `unlink_14`

Directory `nlink` is always **2**, so it does not encode the subdirectory count;
and `unlink` of an open file does not drop `nlink` to 0.

**Measured, because it looked worse than it is:** GNU `find`'s leaf optimization
uses directory `nlink`, and a constant 2 could have made `find` skip descending
entirely. It does not — `find` over `django/contrib/admin` returns 599 files,
exactly matching `git ls-tree -r`. Recorded as a divergence with a named
consumer, not as a live breakage.

### 8. Large files and `EFBIG` — 3 files

`open_25`, `truncate_12`, `ftruncate_12`

Files over 2 GB, and truncate past the maximum file size not returning
`EFBIG`/`EINVAL`. Not investigated; the blob-size ceiling in `limits.rs` is the
likely explanation and this deserves its own pass.

## What was fixed

### `EIO` for a path longer than the filesystem allows — fixed

Root cause: `crates/xvfs-overlay/src/error.rs`'s blanket
`From<XvfsError> for OverlayError` mapped **every** service error to
`Condition::Io`, on the stated premise that "a service error reaching the overlay
is always a storage or protocol failure". That premise is wrong for exactly one
code. The overlay calls `BytePath::validate` on its own arguments, so the
`InvalidArgument` it raises there never crossed a wire — it is a caller error,
and reporting it as `EIO` told a program its filesystem had failed when its path
was merely too long. The module's own header promises that "a new condition
cannot reach the kernel as a plausible-but-wrong `EIO`"; this conversion was the
hole in that promise.

The fix has three parts: `InvalidArgument` now maps to `Condition::Invalid`;
a new `Condition::NameTooLong` carries `ENAMETOOLONG`; and `path_condition` asks
the length question *before* the malformedness question, because the service
vocabulary spells both `InvalidArgument` and POSIX spells them differently.

**The tests still fail, and now they fail honestly.** pjdfstest builds a path just
under the `_PC_PATH_MAX` the filesystem advertises and expects the create to
succeed. XVFS applies its 4 096-byte cap to the path **from the workspace root**,
while POSIX applies `PATH_MAX` to the pathname handed to the syscall — so a file
reachable as a short relative path can still be over XVFS's limit. That is a
real and deliberate difference in what the limit *means*, and the remaining
`ENOENT` assertions in those files are cascades from the first one. The bug was
the errno; the limit is policy, and it is now reported in a form a caller can act
on.

### Directory mtime and ctime inert under mutation — fixed

Root cause: nothing advanced a directory's timestamps when its contents changed,
and `getattr` is answered inline from the inode table, so even a committed
overlay row would not have been reported. Both halves were needed:
`Overlay::touch_directory` adopts the parent and stamps it, and `Xvfs::touch_parent`
**republishes** the inode record — without the second, the first looks like it
did nothing at all.

It runs on create, mkdir, symlink, unlink, rmdir, and both ends of a rename. The
cost is one journal row per directory a job writes into, bounded by the edit set
rather than the repository, and it is invisible downstream: `Status` skips
directory rows outright because Git records no directories, so an adopted parent
produces no change, no diff hunk, and nothing in an export. A test asserts that.

`rmdir/00` and `symlink/00` are now clean.

## What is a decision, not a defect

### `atime` is not stored — needs a product call

Every remaining `utimensat` failure reduces to this. `attr.rs` reports
`atime: mtime`, and `setattr` ignores its `_atime` argument entirely, so a
program that sets atime and reads it back gets mtime instead — `utimensat/09`
sets atime to 2³¹ and mtime to 2³², and reads 2³² back from both.

Fixing it is not a bug fix but a change to what the overlay *models*: a new
column in `entries`, an `OVERLAY_FORMAT_VERSION` bump with no migration
machinery behind it (DESIGN.md section 12 lists crash-safe overlay migration as
pre-production), and a decision about whether XVFS should carry a timestamp Git
cannot record and no export can reproduce. POSIX atime semantics would also mean
every read updates state, which is the cost `noatime` exists to avoid and which a
lazily-hydrating filesystem can least afford.

Two smaller things sit behind the same question: sub-second precision is replaced
by the overlay's own clock tick (`utimensat/08`), and a requested time below the
snapshot floor is clamped, which ADR 0006 already documents as deliberate
(`utimensat/04`).

The current behaviour is the shape this project avoids elsewhere — succeeding
while quietly doing something else. If atime is out of scope, `utimensat` asking
for one should probably say so rather than silently mirror mtime.

### Three more to state rather than fix

The `EPERM` errno for unsupported object types where `EOPNOTSUPP` is more usual;
`chmod` returning success while rounding to Git's two modes; and directory
`nlink` always reading 2.

## xfstests is still not run

This closes half the M2.4 gap and should be recorded as half.

xfstests is oriented at block filesystems: it wants a scratch device and a test
device, and a large share of its `generic` group exercises behaviour XVFS does
not claim — quotas, fallocate ranges, reflink, block-level fsync semantics, DAX,
and crash-consistency against a device. Running it needs a decision about which
subset is meaningful for a FUSE overlay whose base is immutable, and that
decision is a piece of work in itself rather than a script invocation.

The honest next step is to scope that subset explicitly, or to record xfstests as
deliberately not applicable with the reasoning written down — the way M4.4's
token search was recorded as deliberately skipped, rather than left looking
accidental.
