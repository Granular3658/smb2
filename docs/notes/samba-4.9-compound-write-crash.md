# Samba up to 4.15 crashes on a FLUSH inside a compound

**Status**: root-caused (2026-10-19). A Samba bug, avoided on our side since the compound write ends its chain on the
FLUSH (AGENTS.md pitfall 33). The file name keeps its first sighting, on Samba 4.9.5.

The compound write used to send CREATE + WRITE + FLUSH + CLOSE in one frame. On Samba 4.4 through 4.15 (and early
4.16.x / 4.17.x) that panics the server's connection process within a file or two. The connection dies mid-transfer; the
client sees `Disconnected`, `Connection reset by peer`, or `Broken pipe` on whatever it sends next.

## What the server says

```
talloc: access after free error - first free may be at ../tevent_req.c:289
Bad talloc magic value - access after free
PANIC (pid 9435): Bad talloc magic value - access after free
dumping core in /var/log/samba/cores/smbd
```

Samba 4.15.13 prints the same, with `tevent_req.c:291`.

## Why

- MS-SMB2 § 3.3.5.2.7 lets a server fail a compounded operation that needs asynchronous processing unless it's last in
  the chain (product behavior note 266 says Windows does, with `STATUS_INTERNAL_ERROR`).
- Since Samba 4.4 (commit `c737fae5f79f`, 2015), an SMB2 FLUSH is an fsync in Samba's thread pool, so it's always
  async. In `smbd_smb2_request_pending_queue` (`source3/smbd/smb2_server.c`), an async op that isn't a CREATE and isn't
  last in its compound gets `tevent_req_cancel`, and when that fails, `TALLOC_FREE(req->subreq)` and
  `NT_STATUS_INTERNAL_ERROR`. The fsync job is still running in the thread pool, and when it finishes, it touches the
  request just freed: the panic above.
- Samba bug 15172 (commit `26adf3344337`, 2022-11, in 4.18 and backported to 4.16.x and 4.17.x) runs a mid-chain FLUSH
  synchronously, which is why Samba 4.20 and 4.22 take the old chain without complaint. The cancel-and-free path itself
  is still there.
- A FLUSH last in its chain is allowed to go async, and it does (interim `STATUS_PENDING`, then the answer). That shape
  ran clean on 4.10.18 and 4.15.13: 100 writes one at a time and 400 at eight in flight, no panic.

## Reproducing it

- **Samba 4.9.5** (Raspberry Pi 4, Debian buster, 2026-08-01, `examples/write_storm`): 8/8 runs crashed it, the fastest
  126 ms in. Concurrency and size were not the trigger (`WS_CONC=1` and a 4 KB body crashed it); streamed writes, which
  send their FLUSH on its own, never did.
- **Samba 4.15.13** (Alpine 3.15 in Docker, 2026-10-19): the old chain lost the connection on the second file in both runs.
  run. The `smb-samba415` fixture keeps this server in the Docker suite, and `samba415_small_writes_keep_the_connection`
  is the regression test.
- **In the field** (a Cmdr user copying photos to a NAS, SMB 3.1.1, 2026-10-19): every one of seven connection resets
  came 10–110 ms after the client logged the server refusing the FLUSH, and no reset came without one.
- **Immune**: Samba 4.20.6 (the Docker fixtures), 4.22.10 (the Pi since 2026-08-08), and the QNAP TS-464 on current QTS.

## If it comes back

A consumer reporting "the connection dies on small-file copies but large ones are fine" against an older NAS is this
shape. Check that no compound puts an op that can go async (FLUSH first of all) anywhere but last, then check the
server's Samba version.
