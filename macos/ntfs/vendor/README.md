These crates are copied from VOLUMA (https://github.com/festomanolo/voluma, MIT, see LICENSE-voluma):
`block-device`, `fs-core` and `fs-ntfs` (the NTFS read/write driver). The only change is a small
`geometry()` / `cluster_bitmap()` addition at the end of `fs-ntfs/src/lib.rs`, used by `ndaga-ntfs stream`.
