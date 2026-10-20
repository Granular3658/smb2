# How Samba shows a Unix symlink to an SMB2 client

A Windows junction or symlink is a reparse point, and MS-SMB2 says how a client reaches the link itself:
`FILE_OPEN_REPARSE_POINT` in the CREATE (§ 2.2.13). A symlink on a Samba share's disk isn't one, and what
Samba does with it depends on the version. Measured here because it decides whether deleting a link can
cost the files it points to.

## The probe

A guest share holding `rp/target/keep.txt`, `rp/dirlink -> target`, and `rp/filelink -> target/keep.txt`
(made with `ln -s` inside the container), default `follow symlinks = yes`, target inside the share. Read
through `smb2-cli` (`ls --json`, `stat --json`, `rm`, and `rmdir` pointed at each call in turn), comparing
the CREATE with and without `FILE_OPEN_REPARSE_POINT`. 2026-10-20, smb2 0.34.0 development tree.

- Samba 4.20.6: the `smb-guest` fixture (`alpine:3.21`).
- Samba 4.23.10: the same `smb.conf` on `alpine:edge`.

## What each version does

**Listings show neither link on either version.** `dirlink` lists as a plain directory and `filelink` as a
plain 3-byte file, no `REPARSE_POINT` attribute, so `DirectoryEntry::is_link` is `false` and nothing in a
listing tells a consumer not to walk into `dirlink`.

**Samba 4.20.6 ignores `FILE_OPEN_REPARSE_POINT`.** `stat` reports the target either way. `delete_file` on
`filelink` deleted `target/keep.txt` and left the symlink, and `delete_directory` (or `delete_link`) on
`dirlink` then removed the emptied `target/` and left that symlink too. Nothing the client sends reaches
the link.

**Samba 4.23.10 honors it.** With the flag, `stat` on either link reports a non-directory reparse point,
tag `0xA000000C` (`ReparseTag::SYMLINK`), size the length of the link text. `delete_file` on `filelink` and
`delete_link` on `dirlink` removed exactly the links. `delete_directory` on `dirlink` is refused with
`STATUS_NOT_A_DIRECTORY`, because to the server the link is a file; that's why `Tree::delete_link` carries
no file-or-directory condition. **Without the flag (smb2 0.33.0 and earlier), the same `delete_file` deleted
the target's file and the same `delete_directory` removed the target directory**, exactly as on 4.20.

## What it means for a consumer

- The flag is a real fix on current Samba, not only on Windows: before it, deleting a symlink on a 4.23
  share deleted what it pointed to.
- A recursive walk over a Samba share can't rely on the listing to spot a symlinked directory. On 4.23 a
  `stat` of a directory before descending does (`FileInfo::is_link`); on 4.20 nothing does, and a walk that
  deletes as it goes acts on the target's files.
- When a link is known (`is_link` from a listing or a stat), delete it with `delete_link`: it's a directory
  on Windows and a file on Samba.
