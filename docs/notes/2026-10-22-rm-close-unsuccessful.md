# QNAP fails parallel deletes with `STATUS_UNSUCCESSFUL during Close`

**Status**: root-caused (2026-10-22), absorbed client-side in smb2 0.34.1: `Tree::delete_compound` retries once, after
50 ms, when the CLOSE comes back `STATUS_UNSUCCESSFUL` (the fix recommended below). A QNAP server race in its network
recycle bin. Measured on the QNAP TS-464 (QuTS hero, Samba 4.20.0 + QNAP `ksmbd`, `naspi` share,
`recycle bin = yes`), smb2-cli 0.4.26, library 0.34.0.

## Summary

- **Trigger**: two or more connections each delete a file at the same moment, into a folder whose mirror under
  `@Recycle/` doesn't exist yet. QNAP's recycle bin moves every deleted file to `@Recycle/<same path>/<name>`, creating
  the mirror folders on first use. Concurrent first deletes race on creating them; one wins, the others' CLOSE comes
  back `STATUS_UNSUCCESSFUL`.
- **Data**: safe. Every failed file was still in place, intact, and absent from the bin; every reported success was in
  the bin. The error is truthful: "failed" means "not deleted".
- **Who's at fault**: the server. The client's compound is correct, and stock Samba with the same recycle setup returns
  success to the same load (it has its own, worse, fallback; see below).
- **Cmdr**: not exposed through its own work. It deletes one file at a time per volume.
- **Fix**: retry the delete once in the library when the compound's CLOSE returns `STATUS_UNSUCCESSFUL`. Small, and
  every retry in the experiments succeeded.

## Reproduction

`smb2 rm --from-file` on 15 small files, default `-j 8`, into a fresh folder: failed 1, 2, and 7 of 15 in a handful of
runs (7 is what the original report saw), clean in most others. The rate is timing-bound, since the first deletes of each
connection only overlap when the connections finish setup together.

A deterministic version (scratch example, worktree since removed; core loop below): upload 15 files with `smb2 put`,
open N connections, wait on a barrier, then each connection deletes its round-robin share of the files with
`SmbClient::delete_file`, and `stat` every path afterwards.

```rust
// per lane, after connect + connect_share + barrier.wait():
for (pos, p) in lane.into_iter().enumerate() {
    let r = client.delete_file(&tree, &p).await; // record (pos, r)
}
```

Results, 15 files per run, 15 connections, files at `<fresh>/a/b/c/d/e/f/g/h/` (eight new recycle levels):

- Barrier, fresh folder: 11 of 20 runs had failures (most 1–2 files, one run 9 of 15).
- Same, but one file deleted alone first, creating the recycle mirror: 0 of 20 runs.
- Same, inside `@Recycle/` itself (the bin deletes those outright, no move): 0 of 20 runs.
- Same, with one retry of a failed delete on the same connection: 0 final failures in 43 runs.
- Reusing a folder whose recycle mirror already exists: never failed (11 runs, `-j 8`).
- Shallower paths fail less (one new level: a few percent of runs), and file size and file count don't matter.

About 45 failing deletes in all. In the runs with more files than connections (`-j 4` and `-j 8`, where each connection
deletes two or more), every failure was the **first** delete on its connection, and later deletes on the same
connection never failed, because by then the mirror exists. In the 9-of-15 run, the nine failures started
within 1 ms of each other and all completed together ~70 ms later, the shape of several workers blocked on one
directory creation and released as losers.

## What the server does

- **QNAP splits SMB across two servers.** Userland Samba accepts the connection, logs `KSMBD: Add socket`, and hands it
  to the in-kernel `ksmbd`. `/sys/module/ksmbd/parameters/ksmbd_cmd_mask` reads `0x010340`: CLOSE, READ, WRITE, and
  QUERY_INFO run in the kernel; CREATE and SET_INFO in userland Samba. So the delete's CREATE + SET_INFO (disposition)
  and its CLOSE (where the delete actually happens) run in different servers.
- **The recycle bin is QNAP's own code.** `recycle bin = yes` is a QNAP share parameter (`vfs_recycle` isn't in
  `vfs objects`), and duplicates in the bin are versioned `name[2]`, where upstream Samba writes `Copy #1 of name`.
  QNAP's source isn't readable, so the exact failing call is inferred, not seen.
- **After the failed CLOSE, the handle is still open.** The library then sends a standalone CLOSE (it does so after any
  failed op in the compound), and QNAP answers it `STATUS_SUCCESS` without deleting the file. So nothing leaks on the
  server, and the retry's fresh CREATE isn't blocked by a stale open.
- **Why `STATUS_UNSUCCESSFUL`**: in Samba, `map_nt_error_from_unix(0)` (an error path that set no errno) returns
  `UNSUCCESSFUL`, while an unknown errno returns `ACCESS_DENIED` (`source3/lib/errmap_unix.c`, 4.20.0). QNAP's kernel
  path returns the same status, so it's a generic "something below me failed".
- **Server logs show nothing.** smbd runs at log level 0 with a 10 KB cap, and its log at level 5 holds only connection
  accept and hand-off lines, since the ops never reach userland logging. `ksmbd_debug_on` logs only pool bind/unbind.

## Stock Samba, for comparison

The same barrier run against Samba 4.20.6 in Docker (`alpine:3.21`, the fixtures' image), two shares, 20 runs × 15
files each:

- Plain share: 0 failures.
- `vfs objects = recycle`, `recycle:keeptree = yes`: 0 client-visible failures, but the log is full of the same race:
  `recycle_create_dir: recycle: mkdirat failed for @Recycle/run3/a/b/c/d with error: File exists`. Upstream's fallback
  (`vfs_recycle.c` around line 592, "Could not create directory, purging") **deletes the file permanently** instead of
  recycling it: only 4 of 15 files from one run reached the bin. So stock Samba hides the race from the client by
  quietly skipping the safety net; QNAP keeps the file and reports the failure. QNAP's behavior is the safer of the two.

## Other clients

- **macOS smbfs** (the `/Volumes/naspi` mount), 15 parallel `rm` processes on fresh deep folders: 0 failures in 15 runs.
  It carries everything on one connection and its requests don't overlap the way 15 separate connections do, so this
  says little about whether QNAP's race is per-connection or global.
- **`smbclient`**: not installed here (no brew installs), not tested.

## Cmdr exposure

Cmdr uses the same path: `SmbVolume::delete_impl` (`crates/cmdr-smb/src/volume/mutation.rs`) calls
`Tree::delete_link`, which is the same `delete_compound` as `delete_file`. But Cmdr never overlaps two deletes on one
share:

- `SmbVolume` has no `delete_batch_size`, so the delete walker (`crates/cmdr-operations/src/delete/volume_walker.rs`)
  deletes one file at a time, and the default `Volume::delete_files` (used by the move's source sweep) is a sequential
  loop.
- Ops on one volume share a lane with budget 1 (`crates/cmdr-operations/src/manager.rs`), so two Cmdr ops on the same
  share run one after the other.

A Cmdr user hits it only when something else deletes into the same not-yet-recycled folder tree in the same instant
(another Mac, the `smb2` CLI, a sync tool). If it happens, the delete stops at that file with an error, the file is
intact, and running the delete again finishes it.

## Recommended fix

- **Problem**: a parallel delete into a fresh folder on a QNAP share with the recycle bin on fails some files with
  `STATUS_UNSUCCESSFUL during Close`.
- **Impact**: `smb2 rm -j N` (and any library user deleting over several connections) reports false-ish failures and
  exits non-zero; the user has to re-run. No data loss, and Cmdr isn't affected today.
- **Solution**: in `Tree::delete_compound` (or `delete_exclusive_first`), when CREATE and SET_INFO succeeded and the
  CLOSE came back `NtStatus::UNSUCCESSFUL`, keep the existing standalone CLOSE, then run the compound once more, after a
  short pause (~50 ms) to let the winning delete finish creating the folder. Match on the typed status and command, not
  on text. A `NOT_FOUND` on the retry is reported as is (in every observation the first attempt left the file in place,
  so a missing file means someone else removed it). Add a `MockTransport` test (compound with a failing CLOSE, then the
  standalone CLOSE, then a clean compound) and a changelog line. Doing it in the library covers the CLI and Cmdr; a
  retry pass in the CLI's `pool::run` would cover only the CLI.
- **Size**: small. About 20 lines in `crates/smb2/src/client/tree.rs`, one unit test, a CHANGELOG entry, a library patch
  release, and the CLI bump.
- **Clear win or tradeoff**: close to a clear win. It costs one extra round trip only on a failure, and a persistent
  `UNSUCCESSFUL` still surfaces after the one retry. The tradeoff is small: a retry on a catch-all status could, on some
  other server, repeat an operation that failed for an unrelated reason; one bounded retry of an idempotent "open, mark,
  close" keeps that harmless.

Alternatives considered and not recommended: have the CLI delete one file per directory first and fan out after (fixes
only our own concurrency, adds ordering logic, and still races other clients); or do nothing and document it (the CLI is
used for wide batch deletes on exactly this NAS).
