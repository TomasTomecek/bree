//! `bree profile contents` (docs/design.md, "`bree profile contents`"): the
//! inventory of an in-memory render, and the blocks of the roots it leaves
//! unused.

use serde_json::{Value, json};

use crate::profile::Harness;
use crate::render::{self, Kind, Render, harness_dir};

/// One row of the inventory; `file` is empty where the kind has none.
pub struct Item {
    pub file: String,
    pub block: String,
    pub root: String,
}

pub struct Unused {
    pub kind: Kind,
    pub harness: Option<Harness>,
    pub block: String,
    pub root: String,
}

pub struct Inventory {
    pub harness: Harness,
    pub dir: String,
    pub rules: Vec<Item>,
    pub skills: Vec<Item>,
    pub commands: Vec<Item>,
    pub settings: Vec<Item>,
    pub files: Vec<Item>,
    pub unused: Vec<Unused>,
}

pub fn inventory(r: &Render) -> Vec<Inventory> {
    let mut out = Vec::new();
    for h in &r.harnesses {
        // The file a kind renders to, for the kinds with one file.
        let file_of = |kind| {
            h.files
                .iter()
                .find(|f| f.kind == kind)
                .map_or(String::new(), |f| f.path.clone())
        };
        let items = |kind: Kind| -> Vec<Item> {
            h.used
                .iter()
                .filter(|u| u.kind == kind)
                .map(|u| Item {
                    file: match kind {
                        Kind::Rules => file_of(kind),
                        Kind::Commands => format!("{}.md", u.block),
                        _ => u.block.clone(),
                    },
                    block: u.block.clone(),
                    root: u.root.clone(),
                })
                .collect()
        };
        // From the rendered file, not `used`: the seeds of the renderer (the
        // auto mode seed and the tier env vars) can write settings.json with
        // no settings block, and they come first.
        let settings = h
            .files
            .iter()
            .filter(|f| f.kind == Kind::Settings)
            .flat_map(|f| {
                f.sources.iter().map(|s| Item {
                    file: f.path.clone(),
                    block: s.block.clone(),
                    root: s.root.clone(),
                })
            })
            .collect();
        let files = h
            .files
            .iter()
            .filter(|f| f.kind == Kind::Files)
            .map(|f| Item {
                file: f.path.clone(),
                block: f.sources[0].block.clone(),
                root: f.sources[0].root.clone(),
            })
            .collect();
        let mut unused: Vec<Unused> = r
            .library
            .iter()
            .filter(|b| b.harness.is_none_or(|bh| bh == h.harness))
            .filter(|b| !h.used.iter().any(|u| u.kind == b.kind && u.block == b.name))
            .map(|b| Unused {
                kind: b.kind,
                harness: b.harness,
                block: b.name.clone(),
                root: r.roots[b.root].name.clone(),
            })
            .collect();
        unused.sort_by_key(|u| (u.kind.name(), u.block.clone()));
        out.push(Inventory {
            harness: h.harness,
            dir: format!("~/{}", harness_dir(h.harness)),
            rules: items(Kind::Rules),
            skills: items(Kind::Skills),
            commands: items(Kind::Commands),
            settings,
            files,
            unused,
        });
    }
    out
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn row(cells: [&str; 4]) -> Vec<String> {
    let mut r: Vec<String> = cells.iter().map(|c| c.to_string()).collect();
    r[0] = format!("  {}", r[0]);
    r
}

/// The rows of one kind, with the kind's name on the first only.
fn kind_rows(
    label: &str,
    items: &[Item],
    cells: impl Fn(usize, &Item) -> [String; 3],
) -> Vec<Vec<String>> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let [a, b, c] = cells(i, item);
            row([if i == 0 { label } else { "" }, &a, &b, &c])
        })
        .collect()
}

pub fn text(inventories: &[Inventory]) -> String {
    let mut out = String::new();
    for (n, inv) in inventories.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let name = inv.harness.name();
        out.push_str(&format!("{name}  {}\n", inv.dir));
        let mut rows = Vec::new();
        // Rules show the file once, on the first block.
        rows.extend(kind_rows("rules", &inv.rules, |n, i| {
            let file = if n == 0 {
                i.file.clone()
            } else {
                String::new()
            };
            [file, i.block.clone(), i.root.clone()]
        }));
        rows.extend(kind_rows("skills", &inv.skills, |_, i| {
            [i.file.clone(), String::new(), i.root.clone()]
        }));
        rows.extend(kind_rows("commands", &inv.commands, |_, i| {
            [i.file.clone(), String::new(), i.root.clone()]
        }));
        if let Some(first) = inv.settings.first() {
            let blocks: Vec<&str> = inv.settings.iter().map(|i| i.block.as_str()).collect();
            rows.push(row(["settings", &first.file, "", &blocks.join(" < ")]));
        }
        rows.extend(kind_rows("files", &inv.files, |_, i| {
            [
                i.file.clone(),
                String::new(),
                format!("{} ({})", i.block, i.root),
            ]
        }));
        let unused: Vec<Item> = inv
            .unused
            .iter()
            .map(|u| Item {
                file: match u.harness {
                    Some(h) => format!("{}/{}/{}", u.kind.name(), h.name(), u.block),
                    None => format!("{}/{}", u.kind.name(), u.block),
                },
                block: String::new(),
                root: u.root.clone(),
            })
            .collect();
        rows.extend(kind_rows("unused", &unused, |_, i| {
            [i.file.clone(), String::new(), i.root.clone()]
        }));
        out.push_str(&render::align(&rows));
        out.push_str(&format!(
            "{name}: {}, {}, {}, {}, {}, {} unused\n",
            plural(inv.rules.len(), "rule", "rules"),
            plural(inv.skills.len(), "skill", "skills"),
            plural(inv.commands.len(), "command", "commands"),
            plural(
                usize::from(!inv.settings.is_empty()),
                "settings file",
                "settings files"
            ),
            plural(inv.files.len(), "file", "files"),
            inv.unused.len(),
        ));
    }
    out
}

pub fn json(profile: &str, inventories: &[Inventory]) -> String {
    let items = |list: &[Item], key: &str| -> Vec<Value> {
        list.iter()
            .map(|i| match key {
                "" => json!({"block": i.block, "root": i.root}),
                _ => json!({key: i.file, "block": i.block, "root": i.root}),
            })
            .collect()
    };
    let harnesses: Vec<Value> = inventories
        .iter()
        .map(|inv| {
            let unused: Vec<Value> = inv
                .unused
                .iter()
                .map(|u| json!({"kind": u.kind.name(), "block": u.block, "root": u.root}))
                .collect();
            json!({
                "harness": inv.harness.name(),
                "dir": inv.dir,
                "rules": items(&inv.rules, "file"),
                "skills": items(&inv.skills, ""),
                "commands": items(&inv.commands, "file"),
                "settings": items(&inv.settings, "file"),
                "files": items(&inv.files, "path"),
                "unused": unused,
            })
        })
        .collect();
    let mut text =
        serde_json::to_string_pretty(&json!({"profile": profile, "harnesses": harnesses}))
            .expect("serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::env;
    use crate::render::Options;
    use std::path::{Path, PathBuf};

    fn work(roots: &[&str], harnesses: &[Harness], adds: &[&str]) -> Render {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/roots");
        let roots: Vec<PathBuf> = roots.iter().map(|r| base.join(r)).collect();
        let file = base.join("shared/profiles/work.yaml");
        let p = crate::profile::load(&file, "work", &env()).unwrap();
        let adds: Vec<String> = adds.iter().map(|s| s.to_string()).collect();
        // As `bree profile contents` renders: for the container.
        let opts = Options {
            harnesses,
            adds: &adds,
            vars: &[],
            container: true,
        };
        crate::render::render(&p, &file, &roots, &opts).unwrap()
    }

    #[test]
    fn text_lists_every_kind_and_the_unused_blocks() {
        let r = work(&["shared", "private"], &[Harness::Claude], &[]);
        let want = "\
claude  ~/.claude
  rules     CLAUDE.md               core           shared
                                    claude-code    shared
                                    work           shared
                                    work-internal  private
  skills    create-note                            shared
            review-text                            shared
  commands  review-text.md                         shared
  settings  settings.json                          auto mode (bree) < tiers.yaml < work < work-internal
  files     statusline.sh                          statusline (shared)
  unused    commands/release-notes                 shared
            rules/home                             shared
            settings/claude/home                   shared
claude: 4 rules, 2 skills, 1 command, 1 settings file, 1 file, 3 unused
";
        assert_eq!(text(&inventory(&r)), want);
    }

    #[test]
    fn an_absent_optional_block_is_not_listed_and_counts_are_singular() {
        let r = work(&["shared"], &[Harness::Opencode], &[]);
        let out = text(&inventory(&r));
        assert!(!out.contains("work-internal"), "{out}");
        assert!(
            out.contains(
                "opencode: 2 rules, 2 skills, 1 command, 1 settings file, 0 files, 3 unused"
            ),
            "{out}"
        );
    }

    #[test]
    fn add_moves_a_block_out_of_the_unused() {
        let r = work(&["shared"], &[Harness::Claude], &["commands=release-notes"]);
        let out = text(&inventory(&r));
        assert!(out.contains("release-notes.md"), "{out}");
        assert!(!out.contains("unused    commands/release-notes"), "{out}");
    }

    #[test]
    fn json_has_block_and_root_per_entry() {
        let r = work(&["shared", "private"], &[], &[]);
        let v: Value = serde_json::from_str(&json("work", &inventory(&r))).unwrap();
        assert_eq!(v["profile"], "work");
        let hs = v["harnesses"].as_array().unwrap();
        assert_eq!(hs.len(), 3);
        let claude = &hs[0];
        assert_eq!(claude["harness"], "claude");
        assert_eq!(claude["dir"], "~/.claude");
        assert_eq!(
            claude["rules"][3],
            json!({"file": "CLAUDE.md", "block": "work-internal", "root": "private"})
        );
        assert_eq!(
            claude["skills"][0],
            json!({"block": "create-note", "root": "shared"})
        );
        // The seeds of the renderer, then the blocks.
        assert_eq!(
            claude["settings"][0],
            json!({"file": "settings.json", "block": "auto mode (bree)", "root": "shared"})
        );
        assert_eq!(
            claude["settings"][1],
            json!({"file": "settings.json", "block": "tiers.yaml", "root": "shared"})
        );
        assert_eq!(claude["settings"][3]["block"], "work-internal");
        assert_eq!(
            claude["files"][0],
            json!({"path": "statusline.sh", "block": "statusline", "root": "shared"})
        );
        assert_eq!(
            claude["unused"][0],
            json!({"kind": "commands", "block": "release-notes", "root": "shared"})
        );
    }

    #[test]
    fn settings_written_only_for_the_tier_env_are_listed() {
        let t = crate::config::testutil::TempDir::new("contents-tierenv");
        let root = t.0.join("r");
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        std::fs::write(
            root.join("tiers.yaml"),
            "pools:\n  other:\n    big: {model: glm}\n",
        )
        .unwrap();
        let file = root.join("profiles/p.yaml");
        std::fs::write(&file, "harness: claude\npool: other\n").unwrap();
        let p = crate::profile::load(&file, "p", &env()).unwrap();
        // For the host, so that the tier env vars are the only seed.
        let opts = Options {
            harnesses: &[],
            adds: &[],
            vars: &[],
            container: false,
        };
        let r = crate::render::render(&p, &file, std::slice::from_ref(&root), &opts).unwrap();
        let out = text(&inventory(&r));
        assert!(
            out.contains("  settings  settings.json    tiers.yaml\n"),
            "{out}"
        );
        assert!(out.contains("1 settings file"), "{out}");

        // For the container, the auto mode seed comes first, in the root that
        // holds the profile.
        let opts = Options {
            container: true,
            ..opts
        };
        let r = crate::render::render(&p, &file, &[root], &opts).unwrap();
        let out = text(&inventory(&r));
        assert!(
            out.contains("  settings  settings.json    auto mode (bree) < tiers.yaml\n"),
            "{out}"
        );
        assert!(out.contains("1 settings file"), "{out}");
        let v: Value = serde_json::from_str(&json("p", &inventory(&r))).unwrap();
        assert_eq!(
            v["harnesses"][0]["settings"][0],
            json!({"file": "settings.json", "block": "auto mode (bree)", "root": "r"})
        );
    }

    #[test]
    fn settings_written_only_for_the_auto_mode_seed_are_listed() {
        let t = crate::config::testutil::TempDir::new("contents-automode");
        let root = t.0.join("shared");
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        let file = root.join("profiles/p.yaml");
        std::fs::write(&file, "harness: claude\n").unwrap();
        let p = crate::profile::load(&file, "p", &env()).unwrap();
        let opts = Options {
            harnesses: &[],
            adds: &[],
            vars: &[],
            container: true,
        };
        let r = crate::render::render(&p, &file, &[root], &opts).unwrap();
        let out = text(&inventory(&r));
        assert!(
            out.contains("  settings  settings.json    auto mode (bree)\n"),
            "{out}"
        );
        assert!(
            out.contains("claude: 0 rules, 0 skills, 0 commands, 1 settings file,"),
            "{out}"
        );
    }
}
