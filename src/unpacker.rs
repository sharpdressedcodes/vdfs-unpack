use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

const S_IFMT: u16 = 0xF000;
const S_IFLNK: u16 = 0xA000;

use crate::disk::*;
use crate::cattree::{open_cat_tree, cattree_get_first_child};
use crate::decompress::decompress_file;
use crate::extents::{
    build_extent_overflow_map, extents_read_data, extents_resolve_fork, open_ext_tree,
};
use crate::reader::ImageReader;

type OverflowMap = HashMap<u64, Vec<(u64, Vdfs4Extent)>>;

pub fn unpack_filesystem(reader: &ImageReader, output_dir: &Path) -> Result<()> {
    let cat_tree = open_cat_tree(reader)?;

    fs::create_dir_all(output_dir)
        .context("Failed to create output directory")?;

    // Build an index from object_id -> catalog record (file or folder record
    // carrying the real metadata). Used to resolve internal/hard links.
    let all_records = cat_tree.collect_all_records()?;
    let mut by_object: HashMap<u64, Vec<u8>> = HashMap::new();
    for rec in &all_records {
        let rtype = cattree_key_record_type(rec);
        if rtype == VDFS4_CATALOG_FILE_RECORD || rtype == VDFS4_CATALOG_FOLDER_RECORD {
            let oid = cattree_key_object_id(rec);
            by_object.insert(oid, rec.clone());
        }
    }

    // Resolve overflow extents once (files with > 9 extents keep the rest in
    // the extents tree).
    let overflow: OverflowMap = match open_ext_tree(reader) {
        Ok(ext_tree) => build_extent_overflow_map(reader, &ext_tree)?,
        Err(_) => HashMap::new(),
    };

    process_directory(
        reader,
        &cat_tree,
        &by_object,
        &overflow,
        VDFS4_ROOTDIR_OBJ_ID,
        output_dir,
        &mut HashSet::new(),
    )?;

    Ok(())
}

fn process_directory(
    reader: &ImageReader,
    cat_tree: &crate::btree::Btree,
    by_object: &HashMap<u64, Vec<u8>>,
    overflow: &OverflowMap,
    dir_id: u64,
    output_path: &Path,
    visited: &mut HashSet<u64>,
) -> Result<()> {
    if !visited.insert(dir_id) {
        return Ok(());
    }

    let children = cattree_get_first_child(cat_tree, dir_id)
        .context("Failed to get first child")?;

    for child_rec in children {
        let record_type = cattree_key_record_type(&child_rec);
        let name = cattree_key_name_str(&child_rec).to_string();

        match record_type {
            VDFS4_CATALOG_FOLDER_RECORD => {
                let object_id = cattree_key_object_id(&child_rec);
                let child_path = output_path.join(&name);
                if child_path.is_file() {
                    fs::remove_file(&child_path)
                        .with_context(|| format!("remove conflicting file: {}", child_path.display()))?;
                }
                fs::create_dir_all(&child_path)
                    .with_context(|| format!("Failed to create dir: {}", child_path.display()))?;
                process_directory(
                    reader, cat_tree, by_object, overflow, object_id, &child_path, visited,
                )?;
            }
            VDFS4_CATALOG_FILE_RECORD => {
                let object_id = cattree_key_object_id(&child_rec);
                let key_len = cattree_key_key_len(&child_rec);
                let value_data = &child_rec[key_len..];
                let child_path = output_path.join(&name);
                if let Err(e) = extract_file(reader, overflow, object_id, value_data, &child_path) {
                    eprintln!("EXTRACT-FAIL {}: {:#}", child_path.display(), e);
                }
            }
            VDFS4_CATALOG_ILINK_RECORD | VDFS4_CATALOG_HLINK_RECORD => {
                if let Err(e) =
                    resolve_and_extract(reader, by_object, overflow, &child_rec, output_path)
                {
                    eprintln!("LINK-FAIL {}: {:#}", name, e);
                }
            }
            _ => {
                eprintln!("Skipping unknown record type {} for {}", record_type, name);
            }
        }
    }

    Ok(())
}

fn resolve_and_extract(
    reader: &ImageReader,
    by_object: &HashMap<u64, Vec<u8>>,
    overflow: &OverflowMap,
    link_rec: &[u8],
    output_path: &Path,
) -> Result<()> {
    let link_object = cattree_key_object_id(link_rec);
    let name = cattree_key_name_str(link_rec).to_string();
    let child_path = output_path.join(&name);

    // The link's object_id field is the target file's object_id. Look it up in
    // the index built from the real file records.
    match by_object.get(&link_object) {
        Some(target_rec) => {
            let target_type = cattree_key_record_type(target_rec);
            let key_len = cattree_key_key_len(target_rec);
            let target_value = &target_rec[key_len..];

            match target_type {
                VDFS4_CATALOG_FILE_RECORD => {
                    let target_object = cattree_key_object_id(target_rec);
                    extract_file(reader, overflow, target_object, target_value, &child_path)?;
                }
                VDFS4_CATALOG_FOLDER_RECORD => {
                    // Directory hard link: create the directory entry but do
                    // not recurse, to avoid duplicating the target subtree
                    // (the real folder is extracted separately).
                    if child_path.is_file() {
                        fs::remove_file(&child_path)
                            .with_context(|| format!("remove conflicting file: {}", child_path.display()))?;
                    }
                    fs::create_dir_all(&child_path)
                        .with_context(|| format!("Failed to create dir: {}", child_path.display()))?;
                }
                _ => {}
            }
        }
        None => {
            eprintln!("Could not resolve link {} (obj={})", name, link_object);
        }
    }

    Ok(())
}

fn extract_file(
    reader: &ImageReader,
    overflow: &OverflowMap,
    object_id: u64,
    value_data: &[u8],
    output_path: &Path,
) -> Result<()> {
    if value_data.len() < std::mem::size_of::<Vdfs4CatalogFileRecord>() {
        fs::write(output_path, b"").with_context(|| format!("write (empty) failed: {}", output_path.display()))?;
        return Ok(());
    }

    let file_rec: Vdfs4CatalogFileRecord = unsafe {
        std::ptr::read_unaligned(value_data.as_ptr() as *const Vdfs4CatalogFileRecord)
    };

    let fork = file_rec.fork;
    let flags = file_rec.common.flags;
    let file_mode = file_rec.common.file_mode;
    let file_size = fork.size_in_bytes;

    // Symbolic link: the target path is stored inline (fast symlink) or in
    // extents (page symlink). Extract it as an actual symlink so that
    // e.g. `bin -> usr/bin` is recreated as a link rather than a regular file.
    if (file_mode & S_IFMT) == S_IFLNK {
        if output_path.is_symlink() {
            fs::remove_file(output_path)
                .with_context(|| format!("remove conflicting symlink: {}", output_path.display()))?;
        } else if output_path.exists() {
            if output_path.is_dir() {
                fs::remove_dir_all(output_path)
                    .with_context(|| format!("remove conflicting dir: {}", output_path.display()))?;
            } else {
                fs::remove_file(output_path)
                    .with_context(|| format!("remove conflicting file: {}", output_path.display()))?;
            }
        }

        let target = if fork.total_blocks_count == 0 {
            let inline = fork.inline_data();
            let len = (file_size as usize).min(inline.len());
            String::from_utf8_lossy(&inline[..len]).to_string()
        } else {
            let extents = extents_resolve_fork(&fork, file_size, overflow, object_id)?;
            let data = extents_read_data(reader, &extents, 0, file_size)?;
            String::from_utf8_lossy(&data).to_string()
        };
        let target = target.trim_end_matches('\0');
        symlink(target, output_path)
            .with_context(|| format!("create symlink {} -> {}", output_path.display(), target))?;
        return Ok(());
    }

    if file_size == 0 {
        fs::write(output_path, b"").with_context(|| format!("write (zero) failed: {}", output_path.display()))?;
        return Ok(());
    }

    // Handle name collision: if a directory exists at this path (e.g. from a
    // hard link resolving to a folder), remove it before writing the file.
    if output_path.is_dir() {
        if is_dir_empty(output_path) {
            fs::remove_dir_all(output_path)
                .with_context(|| format!("remove conflicting dir: {}", output_path.display()))?;
        } else {
            eprintln!(
                "Skipping file {}: path already exists as a non-empty directory",
                output_path.display()
            );
            return Ok(());
        }
    } else if output_path.is_file() {
        fs::remove_file(output_path)
            .with_context(|| format!("remove conflicting file: {}", output_path.display()))?;
    }

    if crate::decompress::is_compressed_file(flags) {
        let data = decompress_file(reader, &fork, object_id, overflow)?;
        fs::write(output_path, &data)
            .with_context(|| format!("write (compressed) failed: {}", output_path.display()))?;
    } else if fork.total_blocks_count == 0 {
        let inline = fork.inline_data();
        let len = (file_size as usize).min(inline.len());
        fs::write(output_path, &inline[..len])
            .with_context(|| format!("write (inline) failed: {}", output_path.display()))?;
    } else {
        let extents = extents_resolve_fork(&fork, file_size, overflow, object_id)?;
        let data = extents_read_data(reader, &extents, 0, file_size)?;
        fs::write(output_path, &data)
            .with_context(|| format!("write (extents) failed: {}", output_path.display()))?;
    }

    Ok(())
}

fn is_dir_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}
