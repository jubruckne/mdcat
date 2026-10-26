//! Compact directory groups in the link picker while retaining destination indices.

pub struct PickerRow {
    pub label: String,
    pub target: Option<usize>,
}

#[derive(Default)]
struct Node {
    name: String,
    target: Option<usize>,
    count: usize,
    children: Vec<Node>,
}

impl Node {
    fn insert(&mut self, directories: &[&str], file: &str, target: usize) {
        self.count += 1;
        if let Some((directory, rest)) = directories.split_first() {
            let index = self
                .children
                .iter()
                .position(|child| child.target.is_none() && child.name == *directory)
                .unwrap_or_else(|| {
                    self.children.push(Node {
                        name: (*directory).into(),
                        ..Default::default()
                    });
                    self.children.len() - 1
                });
            self.children[index].insert(rest, file, target);
        } else {
            self.children.push(Node {
                name: file.into(),
                target: Some(target),
                count: 1,
                children: Vec::new(),
            });
        }
    }
}

struct Item {
    label: String,
    target: Option<usize>,
    children: Vec<Item>,
}

fn compact(nodes: &[Node], prefix: &str) -> Vec<Item> {
    let mut items = Vec::new();
    for node in nodes {
        let mut label = format!("{prefix}{}", node.name);
        if node.target.is_some() {
            items.push(Item {
                label,
                target: node.target,
                children: Vec::new(),
            });
        } else if node.count >= 3 && node.name != "./" && node.name != "/" {
            let mut branch = node;
            // A chain shared by the entire group belongs on one bold header.
            while branch.children.len() == 1 && branch.children[0].target.is_none() {
                branch = &branch.children[0];
                label.push_str(&branch.name);
            }
            items.push(Item {
                label,
                target: None,
                children: compact(&branch.children, ""),
            });
        } else {
            items.extend(compact(&node.children, &label));
        }
    }
    items
}

fn append_rows(items: &[Item], indent: &str, root: bool, rows: &mut Vec<PickerRow>) {
    for (index, item) in items.iter().enumerate() {
        let last = index + 1 == items.len();
        let connector = if root {
            ""
        } else if last {
            "╰ "
        } else {
            "├ "
        };
        rows.push(PickerRow {
            label: format!("{indent}{connector}{}", item.label),
            target: item.target,
        });
        if !item.children.is_empty() {
            let continuation = if root {
                ""
            } else if last {
                "  "
            } else {
                "│ "
            };
            append_rows(
                &item.children,
                &format!("{indent}{continuation}"),
                false,
                rows,
            );
        }
    }
}

pub fn grouped_rows(labels: &[String]) -> Vec<PickerRow> {
    let mut root = Node::default();
    for (target, label) in labels.iter().enumerate() {
        // A heading fragment may contain slashes; it is part of the file label.
        let path_end = label.find('#').unwrap_or(label.len());
        let directory_end = label[..path_end].rfind('/').map_or(0, |index| index + 1);
        let directories = label[..directory_end]
            .split_inclusive('/')
            .collect::<Vec<_>>();
        root.insert(&directories, &label[directory_end..], target);
    }
    let mut rows = Vec::new();
    append_rows(&compact(&root.children, ""), "", true, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).into()).collect()
    }

    #[test]
    fn shared_chains_and_nested_groups_are_compacted_only_above_two_links() {
        let rows = grouped_rows(&labels(&[
            "./scratch/project/a/base/one.glsl:172",
            "./scratch/project/a/base/two.glsl",
            "./scratch/project/a/other/three.glsl",
            "./scratch/project/b/one.glsl",
            "./scratch/project/b/two.glsl",
            "./scratch/project/b/three.glsl",
            "./scratch/project/receipt.json",
        ]));
        let text = rows
            .iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            [
                "./scratch/project/",
                "├ a/",
                "│ ├ base/one.glsl:172",
                "│ ├ base/two.glsl",
                "│ ╰ other/three.glsl",
                "├ b/",
                "│ ├ one.glsl",
                "│ ├ two.glsl",
                "│ ╰ three.glsl",
                "╰ receipt.json"
            ]
        );
        assert_eq!(
            rows.iter().filter_map(|row| row.target).collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unrelated_groups_preserve_correct_target_indices_and_two_links_stay_flat() {
        let rows = grouped_rows(&labels(&[
            "./a/one.py",
            "./other.txt",
            "./a/two.py",
            "./a/three.py",
            "./b/one.py",
            "./b/two.py",
        ]));
        assert_eq!(
            rows.iter().filter_map(|row| row.target).collect::<Vec<_>>(),
            [0, 2, 3, 1, 4, 5]
        );
        assert_eq!(rows.last().unwrap().label, "./b/two.py");
        assert_eq!(rows.iter().filter(|row| row.target.is_none()).count(), 1);
    }

    #[test]
    fn matching_partial_directory_names_and_heading_slashes_do_not_create_groups() {
        let input = labels(&[
            "./prefix-a/one.md#heading/a",
            "./prefix-b/two.md",
            "./prefix-c/three.md",
        ]);
        let rows = grouped_rows(&input);
        assert!(rows.iter().all(|row| row.target.is_some()));
        assert_eq!(
            rows.iter().map(|row| row.label.clone()).collect::<Vec<_>>(),
            input
        );
    }
}
