//! The tiers manifest (docs/design.md, "Tiers manifest"): `tiers.yaml` at the
//! top of each root, merged across the roots, validated when it is loaded.

use std::fs;
use std::io;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::profile::Harness;
use crate::render::{Root, io_err};
use crate::yaml::{self, Ordered, Spanned};

/// One tier entry: a model, or a fallback to another tier of the pool.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    model: Option<String>,
    name: Option<String>,
    fallback: Option<String>,
    claude: Option<String>,
    pi: Option<String>,
    opencode: Option<String>,
}

impl Tier {
    /// The model id for a harness: its own id, else `model`.
    pub fn id(&self, h: Harness) -> &str {
        let own = match h {
            Harness::Claude => &self.claude,
            Harness::Pi => &self.pi,
            Harness::Opencode => &self.opencode,
        };
        own.as_deref().or(self.model.as_deref()).unwrap_or_default()
    }

    /// The display name: `name`, else the model id.
    pub fn display(&self) -> &str {
        self.name
            .as_deref()
            .or(self.model.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct Pool {
    pub name: String,
    /// The root whose `tiers.yaml` defines the pool.
    pub root: String,
    tiers: Vec<(String, Tier)>,
}

impl Pool {
    pub fn tier_names(&self) -> Vec<&str> {
        self.tiers.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn get(&self, name: &str) -> Option<&Tier> {
        self.tiers.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    /// The tier with a model that `tier` leads to through its fallbacks.
    pub fn resolve(&self, tier: &str) -> Option<&Tier> {
        let mut t = self.get(tier)?;
        while let Some(f) = &t.fallback {
            t = self.get(f)?;
        }
        Some(t)
    }
}

#[derive(Debug, Default)]
pub struct Tiers {
    pub pools: Vec<Pool>,
}

impl Tiers {
    pub fn pool(&self, name: &str) -> Option<&Pool> {
        self.pools.iter().find(|p| p.name == name)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TiersFile {
    #[serde(default)]
    pools: Ordered<Ordered<Spanned<Tier>>>,
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Loads and validates the `tiers.yaml` of every root; the pools are combined
/// in root order, and a pool in two roots is an error.
pub fn load(roots: &[Root]) -> Result<Tiers> {
    let mut all = Tiers::default();
    let mut files: Vec<String> = Vec::new();
    for root in roots {
        let path = root.path.join("tiers.yaml");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&path)(e)),
        };
        let file = path.display().to_string();
        let parsed: TiersFile = yaml::from_str(&text, &file)?;
        for (name, tiers) in parsed.pools.0 {
            if let Some(i) = all.pools.iter().position(|p| p.name == name) {
                return Err(Error::new(format!(
                    "pool `{name}` is defined in both {} and {file}",
                    files[i]
                )));
            }
            validate(&file, &name, &tiers.0)?;
            all.pools.push(Pool {
                name,
                root: root.name.clone(),
                tiers: tiers.0.into_iter().map(|(n, t)| (n, t.value)).collect(),
            });
            files.push(file.clone());
        }
    }
    Ok(all)
}

fn validate(file: &str, pool: &str, tiers: &[(String, Spanned<Tier>)]) -> Result<()> {
    let at = |tier: &str, line: Option<usize>, msg: String| {
        Error::from(yaml::Error::new(
            file,
            line,
            &format!("pools.{pool}.{tier}"),
            msg,
        ))
    };
    if !valid_name(pool) {
        return Err(Error::from(yaml::Error::new(
            file,
            None,
            &format!("pools.{pool}"),
            "a pool name is letters, digits, `-` and `_`",
        )));
    }
    let find = |n: &str| tiers.iter().find(|(t, _)| t == n).map(|(_, t)| &t.value);
    for (name, s) in tiers {
        let (t, line) = (&s.value, Some(s.line));
        if !valid_name(name) {
            return Err(at(
                name,
                line,
                "a tier name is letters, digits, `-` and `_`".into(),
            ));
        }
        let others = [&t.model, &t.name, &t.claude, &t.pi, &t.opencode];
        match &t.fallback {
            Some(_) if others.iter().any(|o| o.is_some()) => {
                return Err(at(
                    name,
                    line,
                    "a fallback tier takes the other tier as a whole: use `fallback` alone".into(),
                ));
            }
            Some(f) if find(f).is_none() => {
                return Err(at(
                    name,
                    line,
                    format!("fallback `{f}` is not a tier of pool `{pool}`"),
                ));
            }
            None if t.model.is_none() => {
                return Err(at(name, line, "a tier needs `model` or `fallback`".into()));
            }
            _ => {}
        }
    }
    for (name, s) in tiers {
        let mut chain = vec![name.as_str()];
        let mut cur = &s.value;
        while let Some(f) = &cur.fallback {
            chain.push(f);
            if chain[..chain.len() - 1].contains(&f.as_str()) {
                let msg = format!("fallback cycle: {}", chain.join(" -> "));
                return Err(at(name, Some(s.line), msg));
            }
            cur = find(f).expect("fallbacks were checked above");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::TempDir;
    use std::path::Path;

    const CLAUDE: &str =
        "pools:\n  claude:\n    big: {model: opus-1, name: Opus}\n    small: {fallback: big}\n";

    fn roots(t: &TempDir, files: &[(&str, &str)]) -> Vec<Root> {
        files
            .iter()
            .map(|(name, text)| {
                let path = t.0.join(name);
                fs::create_dir_all(&path).unwrap();
                if !text.is_empty() {
                    fs::write(path.join("tiers.yaml"), text).unwrap();
                }
                Root {
                    name: name.to_string(),
                    path,
                }
            })
            .collect()
    }

    fn one(tag: &str, text: &str) -> Result<Tiers> {
        let t = TempDir::new(tag);
        load(&roots(&t, &[("r", text)]))
    }

    fn err(tag: &str, text: &str) -> String {
        one(tag, &format!("pools:\n  p:\n{text}")).unwrap_err().0
    }

    #[test]
    fn roots_merge_different_pools_in_order() {
        let t = TempDir::new("merge");
        let rs = roots(
            &t,
            &[
                ("a", CLAUDE),
                ("none", ""),
                ("b", "pools:\n  x:\n    s: {model: m}\n"),
            ],
        );
        let tiers = load(&rs).unwrap();
        let names: Vec<_> = tiers
            .pools
            .iter()
            .map(|p| (p.name.as_str(), p.root.as_str()))
            .collect();
        assert_eq!(names, [("claude", "a"), ("x", "b")]);
        assert!(tiers.pool("y").is_none());
    }

    #[test]
    fn a_pool_in_two_roots_names_both_files() {
        let t = TempDir::new("dup");
        let e = load(&roots(&t, &[("a", CLAUDE), ("b", CLAUDE)]))
            .unwrap_err()
            .0;
        let (a, b) = (t.0.join("a/tiers.yaml"), t.0.join("b/tiers.yaml"));
        let want = |p: &Path| p.display().to_string();
        assert!(
            e.contains("pool `claude`") && e.contains(&want(&a)) && e.contains(&want(&b)),
            "{e}"
        );
    }

    #[test]
    fn fallbacks_resolve_and_ids_and_names_default() {
        let text = "pools:\n  p:\n    big: {model: m, name: Big, pi: prov/m}\n    plain: {model: q}\n    medium: {fallback: big}\n    small: {fallback: medium}\n";
        let tiers = one("resolve", text).unwrap();
        let p = tiers.pool("p").unwrap();
        let small = p.resolve("small").unwrap();
        assert_eq!((small.id(Harness::Claude), small.display()), ("m", "Big"));
        assert_eq!(small.id(Harness::Pi), "prov/m");
        let plain = p.resolve("plain").unwrap();
        assert_eq!((plain.id(Harness::Opencode), plain.display()), ("q", "q"));
        assert!(p.resolve("nope").is_none());
        assert_eq!(p.tier_names(), ["big", "plain", "medium", "small"]);
    }

    #[test]
    fn invalid_entries_are_errors_with_line_and_path() {
        let e = err(
            "cycle",
            "    big: {model: m}\n    medium: {fallback: small}\n    small: {fallback: medium}\n",
        );
        assert!(
            e.contains(":4: pools.p.medium: fallback cycle: medium -> small -> medium"),
            "{e}"
        );
        let e = err("unknown", "    small: {fallback: huge}\n");
        assert!(
            e.contains(":3: pools.p.small: fallback `huge` is not a tier of pool `p`"),
            "{e}"
        );
        let e = err(
            "both",
            "    small: {fallback: big, model: m}\n    big: {model: m}\n",
        );
        assert!(
            e.contains("pools.p.small: a fallback tier takes the other tier as a whole"),
            "{e}"
        );
        let e = err("neither", "    small: {name: S}\n");
        assert!(
            e.contains("pools.p.small: a tier needs `model` or `fallback`"),
            "{e}"
        );
        let e = err("key", "    small: {model: m, colour: red}\n");
        assert!(e.contains("unknown field `colour`"), "{e}");
        let e = err("name", "    'sm all': {model: m}\n");
        assert!(e.contains("pools.p.sm all: a tier name is"), "{e}");
        let e = one("pname", "pools:\n  'a b':\n    s: {model: m}\n")
            .unwrap_err()
            .0;
        assert!(e.contains("pools.a b: a pool name is"), "{e}");
    }
}
