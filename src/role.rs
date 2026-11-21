//! The shared config repository's state file and hashing rule (docs/design.md, "Host target").
//!
//! The role records what it installed in
//! `~/.local/state/ai-role/deployed.json`: an object of the destination
//! relative to HOME and the sha1 of the file. bree reads it while it has no
//! `host.json` of its own, so the first `bree render --target host` after the
//! role does not see every file the role wrote as a conflict. bree never
//! writes or deletes it.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::error::{Error, Result};
use crate::render::{hex, read_optional};

/// The role's state file, relative to the home directory.
const DEPLOYED: &str = ".local/state/ai-role/deployed.json";

/// `DEPLOYED` as messages show it.
pub const DEPLOYED_SHOWN: &str = "~/.local/state/ai-role/deployed.json";

/// `(destination relative to HOME, sha1)` of every file the role installed, or
/// none when the role never ran. A file that is not an object of strings is an
/// error that names it.
pub fn read_deployed(home: &Path) -> Result<Option<BTreeMap<String, String>>> {
    let path = home.join(DEPLOYED);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(None);
    };
    let bad = |what: String| Error::new(format!("{}: {what}", path.display()));
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    let Value::Object(map) = value else {
        return Err(bad("not a JSON object of destination: sha1".to_string()));
    };
    let mut out = BTreeMap::new();
    for (dest, sha1) in map {
        let Value::String(sha1) = sha1 else {
            return Err(bad(format!("`{dest}` is not a string")));
        };
        out.insert(dest, sha1);
    }
    Ok(Some(out))
}

pub fn sha1_hex(bytes: &[u8]) -> String {
    hex(&Sha1::digest(bytes))
}

/// The role's own rule for "the file is what the role wrote": the sha1 of the
/// raw bytes is the recorded one, or, for a `.json` file that parses, so is
/// the sha1 of its `json.dumps(value, sort_keys=True)` form. The second case
/// is what lets a harness rewrite its own settings in another key order.
pub fn matches(path: &str, bytes: &[u8], sha1: &str) -> bool {
    if sha1_hex(bytes).eq_ignore_ascii_case(sha1) {
        return true;
    }
    path.ends_with(".json")
        && serde_json::from_slice::<Value>(bytes)
            .is_ok_and(|v| sha1_hex(python_json(&v).as_bytes()).eq_ignore_ascii_case(sha1))
}

/// What Python's `json.dumps(value, sort_keys=True)` prints: the separators
/// `", "` and `": "`, keys sorted by code point, and every character outside
/// printable ASCII as `\uXXXX` (astral ones as a surrogate pair).
///
/// Numbers use serde_json's formatting. Python prints some floats differently
/// (`1e-05` where serde_json prints `1e-5`), so a file with such a float does
/// not match its role hash, which only makes it a conflict.
pub fn python_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // serde_json keeps insertion order here. `String` order is byte
            // order, which for UTF-8 is code point order, as in Python.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_string(key, out);
                out.push_str(": ");
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            // Other control characters, DEL and everything non-ASCII.
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::TempDir;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn keys_are_sorted_at_every_level_by_code_point() {
        // U+FFFF comes before U+10000 by code point, though not in UTF-16.
        let v = json(
            "{\"b\": 1, \"a\": {\"z\": [3, 2], \"y\": null}, \"\u{10000}\": 2, \"\u{ffff}\": 3, \"\": 4}",
        );
        assert_eq!(
            python_json(&v),
            "{\"\": 4, \"a\": {\"y\": null, \"z\": [3, 2]}, \"b\": 1, \"\\uffff\": 3, \"\\ud800\\udc00\": 2}"
        );
    }

    #[test]
    fn separators_are_comma_space_and_colon_space() {
        let v = json("{\n \"a\": [1, 2, {\"b\":true}],\n\"c\" :false\n}");
        assert_eq!(python_json(&v), r#"{"a": [1, 2, {"b": true}], "c": false}"#);
        assert_eq!(python_json(&json("[]")), "[]");
        assert_eq!(python_json(&json("{}")), "{}");
        assert_eq!(python_json(&json("[{}, []]")), "[{}, []]");
    }

    #[test]
    fn literals_and_numbers() {
        let v = json("[true, false, null, 0, -12, 18446744073709551615, 3.5]");
        assert_eq!(
            python_json(&v),
            "[true, false, null, 0, -12, 18446744073709551615, 3.5]"
        );
    }

    #[test]
    fn strings_are_escaped_like_python() {
        let v = json(r#"["say \"hi\"", "a\\b", "\n\r\t\b\f", "\u0001\u001f\u007f", "/"]"#);
        assert_eq!(
            python_json(&v),
            r#"["say \"hi\"", "a\\b", "\n\r\t\b\f", "\u0001\u001f\u007f", "/"]"#
        );
    }

    #[test]
    fn non_ascii_is_escaped_in_lowercase_hex_with_surrogate_pairs() {
        let v = json("[\"caf\u{e9}\", \"\u{20ac}\", \"\u{1f600}\", \"\u{10ffff}\"]");
        assert_eq!(
            python_json(&v),
            "[\"caf\\u00e9\", \"\\u20ac\", \"\\ud83d\\ude00\", \"\\udbff\\udfff\"]"
        );
        // Keys are escaped the same way.
        assert_eq!(python_json(&json("{\"\u{e9}\": 1}")), "{\"\\u00e9\": 1}");
    }

    #[test]
    fn sha1_of_abc_is_the_known_value() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn raw_bytes_match_for_any_file() {
        let sha = sha1_hex(b"role text\n");
        assert!(matches("CLAUDE.md", b"role text\n", &sha));
        assert!(matches("CLAUDE.md", b"role text\n", &sha.to_uppercase()));
        assert!(!matches("CLAUDE.md", b"role text\n\n", &sha));
        // A non-JSON path never takes the canonical route.
        let sha = sha1_hex(br#"{"a": 1}"#);
        assert!(!matches("notes.md", b"{\"a\":1}", &sha));
    }

    #[test]
    fn json_files_match_in_the_python_form_whatever_the_key_order() {
        let sha = sha1_hex(b"{\"a\": \"caf\\u00e9\", \"b\": [1, 2]}");
        let role = "{\n  \"b\": [1,\n 2],\n  \"a\": \"caf\u{e9}\"\n}\n";
        assert!(matches("settings.json", role.as_bytes(), &sha));
        // The escaped spelling of the same string parses to the same value.
        let escaped = "{\"a\":\"caf\\u00e9\",\"b\":[1,2]}";
        assert!(matches("settings.json", escaped.as_bytes(), &sha));
        assert!(!matches(
            "settings.json",
            br#"{"a":"cafe","b":[1,2]}"#,
            &sha
        ));
        assert!(!matches("settings.json", b"not json", &sha));
        assert!(!matches("settings.jsonc", role.as_bytes(), &sha));
    }

    #[test]
    fn deployed_json_is_read_as_an_object_of_strings() {
        let t = TempDir::new("role-read");
        assert_eq!(read_deployed(&t.0).unwrap(), None);
        let path = t.0.join(DEPLOYED);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{".pi/agent/AGENTS.md": "b", ".claude/CLAUDE.md": "a"}"#,
        )
        .unwrap();
        let got = read_deployed(&t.0).unwrap().unwrap();
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            [
                (".claude/CLAUDE.md".to_string(), "a".to_string()),
                (".pi/agent/AGENTS.md".to_string(), "b".to_string()),
            ]
        );
    }

    #[test]
    fn a_deployed_json_that_is_not_an_object_of_strings_is_an_error_naming_it() {
        let t = TempDir::new("role-bad");
        let path = t.0.join(DEPLOYED);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (text, what) in [
            ("{not json", "at line 1 column"),
            ("[]", "not a JSON object"),
            (
                r#"{".claude/CLAUDE.md": 3}"#,
                "`.claude/CLAUDE.md` is not a string",
            ),
            (r#"{".claude/CLAUDE.md": null}"#, "is not a string"),
        ] {
            std::fs::write(&path, text).unwrap();
            let e = read_deployed(&t.0).unwrap_err();
            assert!(
                e.0.starts_with(&format!("{}: ", path.display())) && e.0.contains(what),
                "{text}: {e}"
            );
        }
    }
}
