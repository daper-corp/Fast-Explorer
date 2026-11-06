//! Search results grouped by the folder tree they came from.
//!
//! A flat list of hits loses where they live. This keeps only the folders on the way to a
//! hit (so folders with no matches never appear) and flattens the expanded part into the
//! list model's row order. Each node index is also the index into the row entries.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use fex_core::search::below;
use fex_core::{Entry, SortKey, sort_order};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Display-only facts for one row of the flattened tree.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub depth: u16,
    pub expandable: bool,
    pub expanded: bool,
}

/// The visible part of a search tree, in row order.
#[derive(Default, Debug)]
pub struct FlatTree {
    /// Node indices in display order.
    pub order: Vec<u32>,
    /// Per node: indentation and expansion state.
    pub metas: Vec<RowMeta>,
    /// Per node: folder shown in the "위치" column.
    pub locations: Vec<String>,
    /// Matching items that are not hidden (folders included).
    pub hits: usize,
}

struct Node {
    path: PathBuf,
    children: Vec<u32>,
    expanded: bool,
    /// The item itself matched (not just a folder on the way to a match).
    hit: bool,
}

/// A search result tree. Node indices are row-entry indices as well.
#[derive(Default)]
pub struct SearchTree {
    nodes: Vec<Node>,
    roots: Vec<u32>,
    by_path: HashMap<String, u32>,
}

impl SearchTree {
    /// Adds the search's base folder as a root and returns its node.
    pub fn root(&mut self, entries: &mut Vec<Entry>, path: &Path) -> u32 {
        let entry = file_entry(path, true);
        self.root_entry(entries, path, entry)
    }

    /// Adds a root with an entry already read off the UI thread.
    pub fn root_entry(&mut self, entries: &mut Vec<Entry>, path: &Path, entry: Entry) -> u32 {
        if let Some(&i) = self.by_path.get(&path_key(path)) {
            return i;
        }
        self.push(entries, None, path.to_path_buf(), entry, false)
    }

    /// Inserts one hit, creating the folders on the way to it. With `base`, the tree starts at
    /// that folder; otherwise each hit starts at its volume root (drive or UNC share).
    pub fn insert(
        &mut self,
        entries: &mut Vec<Entry>,
        path: &Path,
        dir: bool,
        entry: Option<Entry>,
        base: Option<&Path>,
    ) {
        let root_path = base
            .map(Path::to_path_buf)
            .or_else(|| fex_core::search::volume_root(path))
            .unwrap_or_else(|| path.to_path_buf());
        let root = self.root(entries, &root_path);
        let Some(rest) = below(path, &root_path) else {
            return; // not under the scoped folder; the index should not send this
        };
        if rest.is_empty() {
            return; // the search root itself is never a result
        }
        let last = rest.len() - 1;
        let mut parent = root;
        let mut current = root_path;
        let mut leaf = entry;
        for (i, part) in rest.into_iter().enumerate() {
            current.push(part);
            let is_last = i == last;
            let key = path_key(&current);
            parent = match self.by_path.get(&key) {
                Some(&node) => {
                    if is_last {
                        self.nodes[node as usize].hit = true;
                        // A deeper hit may have created this folder as a stub first.
                        if let Some(e) = leaf.take() {
                            entries[node as usize] = e;
                        }
                    }
                    node
                }
                None => {
                    let e = match is_last {
                        true => leaf.take().unwrap_or_else(|| file_entry(path, dir)),
                        false => dir_entry(&current),
                    };
                    self.push(entries, Some(parent), current.clone(), e, is_last)
                }
            };
        }
    }

    fn push(&mut self, entries: &mut Vec<Entry>, parent: Option<u32>, path: PathBuf, entry: Entry, hit: bool) -> u32 {
        let node = entries.len() as u32;
        entries.push(entry);
        self.nodes.push(Node { path: path.clone(), children: Vec::new(), expanded: true, hit });
        match parent {
            Some(p) => self.nodes[p as usize].children.push(node),
            None => self.roots.push(node),
        }
        self.by_path.insert(path_key(&path), node);
        node
    }

    /// The node's full path, for opening/dragging a row.
    pub fn path(&self, node: u32) -> Option<&Path> {
        self.nodes.get(node as usize).map(|n| n.path.as_path())
    }

    /// Whether this node is a search hit rather than a folder shown for context.
    pub fn is_hit(&self, node: u32) -> bool {
        self.nodes.get(node as usize).is_some_and(|n| n.hit)
    }

    /// Expands or collapses a folder; returns false when it has no children.
    pub fn toggle(&mut self, node: u32) -> bool {
        let Some(n) = self.nodes.get_mut(node as usize) else { return false };
        if n.children.is_empty() {
            return false;
        }
        n.expanded = !n.expanded;
        true
    }

    /// Flattens the expanded, non-empty part of the tree for display.
    pub fn flatten(&self, entries: &[Entry], key: SortKey, desc: bool, show_hidden: bool) -> FlatTree {
        let mut flat = FlatTree {
            order: Vec::new(),
            metas: vec![RowMeta::default(); self.nodes.len()],
            locations: vec![String::new(); self.nodes.len()],
            hits: 0,
        };
        let mut roots = self.roots.clone();
        sort_nodes(&mut roots, entries, key, desc);
        for root in roots {
            let (_, hits) = self.collect(
                root,
                0,
                entries,
                key,
                desc,
                show_hidden,
                &mut flat.order,
                &mut flat.metas,
                &mut flat.locations,
            );
            flat.hits += hits;
        }
        flat
    }

    /// Appends the visible rows below `node` to `order`; returns whether this node is
    /// visible and how many matching items it contains.
    #[allow(clippy::too_many_arguments)]
    fn collect(
        &self,
        node: u32,
        depth: u16,
        entries: &[Entry],
        key: SortKey,
        desc: bool,
        show_hidden: bool,
        order: &mut Vec<u32>,
        metas: &mut [RowMeta],
        locations: &mut [String],
    ) -> (bool, usize) {
        let n = &self.nodes[node as usize];
        let mut kids = n.children.clone();
        sort_nodes(&mut kids, entries, key, desc);
        let mut kid_order = Vec::new();
        let mut child_hits = 0;
        let mut any_visible = false;
        for kid in kids {
            let (visible, hits) = self.collect(kid, depth + 1, entries, key, desc, show_hidden, &mut kid_order, metas, locations);
            if visible {
                any_visible = true;
                child_hits += hits;
            }
        }
        let self_hit = n.hit && shown(&entries[node as usize], show_hidden);
        if !self_hit && !any_visible {
            return (false, 0);
        }
        order.push(node);
        metas[node as usize] = RowMeta { depth, expandable: any_visible, expanded: n.expanded };
        locations[node as usize] = n.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
        if n.expanded {
            order.extend(kid_order);
        }
        (true, child_hits + self_hit as usize)
    }
}

fn shown(e: &Entry, show_hidden: bool) -> bool {
    show_hidden || e.attrs & FILE_ATTRIBUTE_HIDDEN == 0
}

fn sort_nodes(ids: &mut Vec<u32>, entries: &[Entry], key: SortKey, desc: bool) {
    if ids.len() < 2 {
        return;
    }
    let sub: Vec<Entry> = ids.iter().map(|&i| entries[i as usize].clone()).collect();
    let order = sort_order(&sub, key, desc);
    *ids = order.iter().map(|&i| ids[i as usize]).collect();
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase()
}

/// A folder entry with no file-system read. Its metadata is not shown, and reading it per
/// hit would put thousands of calls on the UI thread.
pub fn synthetic_folder(path: &Path) -> Entry {
    Entry::new(&node_name(path), 0, 0, FILE_ATTRIBUTE_DIRECTORY)
}

fn dir_entry(path: &Path) -> Entry {
    synthetic_folder(path)
}

/// The entry for one hit path; the UI uses this for index and walk results.
pub fn file_entry(path: &Path, dir: bool) -> Entry {
    let name = node_name(path);
    match std::fs::symlink_metadata(path) {
        Ok(m) => Entry::new(&name, m.file_size(), m.last_write_time() as i64, m.file_attributes()),
        Err(_) => Entry::new(&name, 0, 0, if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 }),
    }
}

fn node_name(path: &Path) -> OsString {
    match path.file_name() {
        Some(name) => name.to_os_string(),
        None => OsString::from(path.display().to_string().trim_end_matches(['\\', '/']).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn entry(name: &str, dir: bool, attrs: u32) -> Entry {
        Entry::new(OsStr::new(name), 0, 0, if dir { FILE_ATTRIBUTE_DIRECTORY } else { attrs })
    }

    fn sample() -> (SearchTree, Vec<Entry>) {
        let mut tree = SearchTree::default();
        let mut entries = Vec::new();
        let base = Path::new(r"C:\");
        tree.root(&mut entries, base);
        tree.insert(&mut entries, Path::new(r"C:\a\one.txt"), false, Some(entry("one.txt", false, 0)), Some(base));
        tree.insert(&mut entries, Path::new(r"C:\a\b\two.txt"), false, Some(entry("two.txt", false, 0)), Some(base));
        tree.insert(&mut entries, Path::new(r"C:\c\three.txt"), false, Some(entry("three.txt", false, 0)), Some(base));
        tree.insert(&mut entries, Path::new(r"C:\a\hidden.txt"), false, Some(entry("hidden.txt", false, FILE_ATTRIBUTE_HIDDEN)), Some(base));
        (tree, entries)
    }

    fn names(_tree: &SearchTree, entries: &[Entry], flat: &FlatTree) -> Vec<String> {
        flat.order.iter().map(|&i| entries[i as usize].name()).collect()
    }

    #[test]
    fn keeps_only_folders_with_matches() {
        let (tree, entries) = sample();
        let flat = tree.flatten(&entries, SortKey::Name, false, false);
        assert_eq!(names(&tree, &entries, &flat), ["C:", "a", "b", "two.txt", "one.txt", "c", "three.txt"]);
        assert_eq!(flat.hits, 3); // hidden.txt is hidden
        let depths: Vec<u16> = flat.order.iter().map(|&i| flat.metas[i as usize].depth).collect();
        assert_eq!(depths, [0, 1, 2, 3, 2, 1, 2]);
        // Folders with matches are expandable; files are not.
        assert!(flat.metas[flat.order[0] as usize].expandable);
        assert!(!flat.metas[flat.order[3] as usize].expandable);
    }

    #[test]
    fn collapsing_hides_children_but_keeps_them_counted() {
        let (mut tree, entries) = sample();
        let flat = tree.flatten(&entries, SortKey::Name, false, false);
        // "b" is the third row.
        let b = flat.order[2];
        assert!(tree.toggle(b));
        let flat = tree.flatten(&entries, SortKey::Name, false, false);
        assert_eq!(names(&tree, &entries, &flat), ["C:", "a", "b", "one.txt", "c", "three.txt"]);
        assert_eq!(flat.hits, 3);
        assert!(!flat.metas[b as usize].expanded);
    }

    #[test]
    fn hidden_matches_disappear_with_their_folders() {
        let mut tree = SearchTree::default();
        let mut entries = Vec::new();
        let base = Path::new(r"C:\");
        tree.root(&mut entries, base);
        tree.insert(&mut entries, Path::new(r"C:\only\hidden.txt"), false, Some(entry("hidden.txt", false, FILE_ATTRIBUTE_HIDDEN)), Some(base));
        let flat = tree.flatten(&entries, SortKey::Name, false, false);
        assert!(flat.order.is_empty());
        assert_eq!(flat.hits, 0);
        let flat = tree.flatten(&entries, SortKey::Name, false, true);
        assert_eq!(names(&tree, &entries, &flat), ["C:", "only", "hidden.txt"]);
        assert_eq!(flat.hits, 1);
    }

    #[test]
    fn roots_grow_per_volume() {
        let mut tree = SearchTree::default();
        let mut entries = Vec::new();
        tree.insert(&mut entries, Path::new(r"C:\x\a.txt"), false, Some(entry("a.txt", false, 0)), None);
        tree.insert(&mut entries, Path::new(r"D:\y\b.txt"), false, Some(entry("b.txt", false, 0)), None);
        let flat = tree.flatten(&entries, SortKey::Name, false, false);
        assert_eq!(names(&tree, &entries, &flat), ["C:", "x", "a.txt", "D:", "y", "b.txt"]);
        assert_eq!(flat.hits, 2);
    }
}
