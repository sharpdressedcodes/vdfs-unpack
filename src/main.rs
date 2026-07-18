#![allow(dead_code)]

mod btree;
mod cattree;
mod decompress;
mod disk;
mod extents;
mod reader;
mod snapshot;
mod superblock;
mod unpacker;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "vdfs-unpack", about = "Extract VDFS4 filesystem images")]
struct Cli {
    input: PathBuf,

    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    if !args.input.exists() {
        bail!("Input file does not exist: {}", args.input.display());
    }

    let image = std::fs::read(&args.input)
        .with_context(|| format!("Failed to read image: {}", args.input.display()))?;

    let output_dir = args.output.unwrap_or_else(|| {
        let stem = args.input.file_stem().unwrap_or_default();
        PathBuf::from(format!("{}-extracted", stem.to_string_lossy()))
    });

    let reader = reader::ImageReader::new(&image)?;

    println!("VDFS4 Image Info:");
    println!("  Block size: {} bytes", reader.sbi.block_size);
    println!("  Volume blocks: {}", reader.sbi.volume_blocks_count);
    println!("  Case insensitive: {}", reader.sbi.case_insensitive);

    println!("Extracting to: {}", output_dir.display());
    unpacker::unpack_filesystem(&reader, &output_dir)?;

    println!("Done!");
    Ok(())
}
