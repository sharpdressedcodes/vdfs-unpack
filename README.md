# vdfs-unpack

A Rust utility for unpacking **Samsung VDFS4** filesystem images (`.img`) into
a regular host directory: it correctly decompresses compressed files and
reconstructs symbolic links (e.g. `bin -> usr/bin`).

## Build and usage

```sh
cargo run --release -- <image.img> -o <output_dir>
```

Arguments:

- `<INPUT>` — path to the VDFS4 image (e.g. `platform.img`).
- `-o, --output <OUTPUT>` — target directory for extraction.

## License

This project is distributed under the MIT license. See the [LICENSE](LICENSE)
file.
