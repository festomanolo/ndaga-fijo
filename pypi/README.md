# Ndaga Fijo

Create bootable USB drives on your Mac from an ISO or disk image. Fast, safe, and Windows-ready.

```
pip install ndaga-fijo
ndaga-fijo            # opens the app
ndaga-fijo --install  # optional: copy it to /Applications
```

- Writes hybrid ISOs and raw images block for block, with optional verification.
- Builds Windows installers on FAT32: an `install.wim` over 4 GB is split into `.swm` parts automatically.
- Installs legacy-BIOS boot code for Windows media (MBR + FAT32), UEFI works from the files alone.
- Only ever lists removable drives, and refuses the disk macOS is running from.
- Needs macOS 13 or later (Apple Silicon and Intel). It asks for your password when it writes to a drive.

Website: https://festomanolo.com/ndaga-fijo/ · Source: https://github.com/festomanolo/ndaga-fijo

Ndaga Fijo is free software (GPL v3 or later) and a derivative work of [Rufus](https://rufus.ie) by Pete Batard
and ms-sys by Henrik Carlqvist. It is not affiliated with or endorsed by those projects.
