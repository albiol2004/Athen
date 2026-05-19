//! Build and read git tree objects from a flat path→blob map.
//!
//! Git stores nested directories as nested tree objects. We get a flat
//! `BTreeMap<PathBuf, ObjectId>` and have to assemble it into the
//! recursive shape git expects. The straightforward recursive build is
//! fine here — snapshot path sets are small (1–10 files typically).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use athen_core::error::{AthenError, Result};
use gix::objs::tree::{Entry, EntryKind};

/// Write a tree (and all its sub-trees) corresponding to the given
/// flat path→blob map. Returns the root tree's oid.
pub fn write_tree(
    repo: &gix::Repository,
    files: &BTreeMap<PathBuf, gix::ObjectId>,
) -> Result<gix::ObjectId> {
    write_subtree(repo, files, &PathBuf::new())
}

fn write_subtree(
    repo: &gix::Repository,
    files: &BTreeMap<PathBuf, gix::ObjectId>,
    prefix: &Path,
) -> Result<gix::ObjectId> {
    use std::collections::BTreeMap as Map;

    // Group entries by their next path component within `prefix`.
    // - Direct children with no further nesting → Blob entries.
    // - Anything deeper → recurse, passing the original full-path keys
    //   along (the recursive call re-applies `strip_prefix` with a
    //   longer `prefix`, so keys must stay rooted at the original).
    let mut direct: Map<String, gix::ObjectId> = Map::new();
    let mut deeper: Map<String, BTreeMap<PathBuf, gix::ObjectId>> = Map::new();

    for (path, oid) in files {
        let Ok(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        let mut comps = rest.components();
        let Some(first) = comps.next() else { continue };
        let first = first.as_os_str().to_string_lossy().into_owned();
        if comps.clone().next().is_none() {
            direct.insert(first, *oid);
        } else {
            deeper.entry(first).or_default().insert(path.clone(), *oid);
        }
    }

    // Recurse first so we have the sub-tree oids ready.
    let mut entries: Vec<Entry> = Vec::with_capacity(direct.len() + deeper.len());
    for (name, sub_files) in &deeper {
        let sub_prefix = prefix.join(name);
        let sub_oid = write_subtree(repo, sub_files, &sub_prefix)?;
        entries.push(Entry {
            mode: EntryKind::Tree.into(),
            filename: name.as_bytes().into(),
            oid: sub_oid,
        });
    }
    for (name, oid) in &direct {
        entries.push(Entry {
            mode: EntryKind::Blob.into(),
            filename: name.as_bytes().into(),
            oid: *oid,
        });
    }
    // Git requires tree entries sorted by name (with directories
    // suffixed by `/` for ordering purposes — gix handles that
    // internally as long as the entries are sorted by `filename`).
    entries.sort_by(|a, b| a.filename.cmp(&b.filename));

    let tree = gix::objs::Tree { entries };
    let oid = repo
        .write_object(&tree)
        .map_err(|e| AthenError::Other(format!("write tree: {e}")))?
        .detach();
    Ok(oid)
}

/// Walk a tree recursively and return a flat (path, blob_oid) list.
/// Inverse of `write_tree`. Paths returned are relative — same shape
/// as the keys originally passed in.
pub fn flatten_tree(
    repo: &gix::Repository,
    root: gix::ObjectId,
) -> Result<Vec<(PathBuf, gix::ObjectId)>> {
    let mut out = Vec::new();
    walk(repo, root, &PathBuf::new(), &mut out)?;
    Ok(out)
}

fn walk(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    prefix: &Path,
    out: &mut Vec<(PathBuf, gix::ObjectId)>,
) -> Result<()> {
    let obj = repo
        .find_object(tree_id)
        .map_err(|e| AthenError::Other(format!("find tree {tree_id}: {e}")))?;
    let tree = obj.into_tree();
    for entry in tree.iter() {
        let entry = entry.map_err(|e| AthenError::Other(format!("read tree entry: {e}")))?;
        let name = String::from_utf8_lossy(entry.filename()).into_owned();
        let child_path = prefix.join(&name);
        let kind: EntryKind = entry.mode().into();
        match kind {
            EntryKind::Blob | EntryKind::BlobExecutable => {
                out.push((child_path, entry.oid().to_owned()));
            }
            EntryKind::Tree => {
                walk(repo, entry.oid().to_owned(), &child_path, out)?;
            }
            _ => {
                // Symlinks/commits not produced by our writer. Ignore.
            }
        }
    }
    Ok(())
}
