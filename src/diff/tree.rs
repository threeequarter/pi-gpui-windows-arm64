use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug)]
pub(super) struct ChangeRow {
    pub(super) depth: usize,
    pub(super) kind: ChangeRowKind,
}

#[derive(Clone, Debug)]
pub(super) enum ChangeRowKind {
    Folder {
        path: String,
        name: String,
        subtree_end: usize,
    },
    File(usize),
}

#[derive(Default)]
struct Directory<'a> {
    folders: BTreeMap<&'a str, Directory<'a>>,
    files: Vec<(&'a str, usize)>,
}

impl Directory<'_> {
    fn append_children(mut self, parent: &str, depth: usize, rows: &mut Vec<ChangeRow>) {
        for (name, mut folder) in self.folders {
            let mut name = name.to_owned();
            while folder.files.is_empty() && folder.folders.len() == 1 {
                let (child_name, child) = folder.folders.pop_first().unwrap();
                name.push('/');
                name.push_str(child_name);
                folder = child;
            }
            let path = if parent.is_empty() {
                name.clone()
            } else {
                format!("{parent}/{name}")
            };
            let index = rows.len();
            rows.push(ChangeRow {
                depth,
                kind: ChangeRowKind::Folder {
                    path: path.clone(),
                    name,
                    subtree_end: 0,
                },
            });
            folder.append_children(&path, depth + 1, rows);
            let end = rows.len();
            if let ChangeRowKind::Folder { subtree_end, .. } = &mut rows[index].kind {
                *subtree_end = end;
            }
        }
        self.files.sort_unstable();
        rows.extend(self.files.into_iter().map(|(_, index)| ChangeRow {
            depth,
            kind: ChangeRowKind::File(index),
        }));
    }
}

pub(super) fn build_rows<'a>(paths: impl ExactSizeIterator<Item = &'a str>) -> Vec<ChangeRow> {
    let mut rows = Vec::with_capacity(paths.len());
    let mut root = Directory::default();
    for (index, path) in paths.enumerate() {
        let mut directory = &mut root;
        let name = if let Some((parent, name)) = path.rsplit_once('/') {
            for component in parent.split('/') {
                directory = directory.folders.entry(component).or_default();
            }
            name
        } else {
            path
        };
        directory.files.push((name, index));
    }
    root.append_children("", 0, &mut rows);
    rows
}

pub(super) fn visible_rows(rows: &[ChangeRow], collapsed: Option<&HashSet<String>>) -> Vec<usize> {
    let Some(collapsed) = collapsed.filter(|paths| !paths.is_empty()) else {
        return (0..rows.len()).collect();
    };
    let mut visible = Vec::new();
    let mut index = 0;
    while let Some(row) = rows.get(index) {
        visible.push(index);
        index = match &row.kind {
            ChangeRowKind::Folder {
                path, subtree_end, ..
            } if collapsed.contains(path) => *subtree_end,
            _ => index + 1,
        };
    }
    visible
}
