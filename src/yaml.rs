//! The strict YAML subset of docs/design.md ("File formats").
//!
//! yaml-rust2's event parser reads the text; this module builds a small tree
//! from the events, rejects what the design excludes (several documents,
//! anchors, aliases, tags, merge keys, duplicate keys), and offers a serde
//! `Deserializer` over the tree whose errors carry the file, the line and the
//! key path (`work.yaml:12: container.kvm: expected true or false, found `1``).

use std::fmt;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

/// A parsed value and the 1-based line it starts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Null,
    Scalar { value: String, plain: bool },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

/// An error with the place it happened. `Display` gives
/// `<file>:<line>: <key path>: <message>`, leaving out what is not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    file: String,
    line: Option<usize>,
    path: String,
    message: String,
}

impl Error {
    pub fn new(file: &str, line: Option<usize>, path: &str, message: impl Into<String>) -> Self {
        Error {
            file: file.to_string(),
            line,
            path: path.to_string(),
            message: message.into(),
        }
    }

    /// Adds a location unless the error has one already (the innermost wins).
    fn at(mut self, line: usize, path: &str) -> Self {
        if self.line.is_none() {
            self.line = Some(line);
            self.path = path.to_string();
        }
        self
    }

    fn in_file(mut self, file: &str) -> Self {
        self.file = file.to_string();
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.file)?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        f.write_str(": ")?;
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::new("", None, "", msg.to_string())
    }
}

// ---------------------------------------------------------------- parsing

enum Frame {
    Seq {
        line: usize,
        items: Vec<Node>,
    },
    Map {
        line: usize,
        entries: Vec<(Node, Node)>,
        key: Option<Node>,
    },
}

struct Builder<'a> {
    file: &'a str,
    stack: Vec<Frame>,
    root: Option<Node>,
}

impl Builder<'_> {
    fn err(&self, line: usize, message: impl Into<String>) -> Error {
        Error::new(self.file, Some(line), "", message)
    }

    fn check_props(&self, anchor: usize, tag: bool, line: usize) -> Result<(), Error> {
        if anchor != 0 {
            return Err(self.err(line, "anchors are not supported"));
        }
        if tag {
            return Err(self.err(line, "tags are not supported"));
        }
        Ok(())
    }

    fn push_value(&mut self, node: Node) -> Result<(), Error> {
        match self.stack.last_mut() {
            None => self.root = Some(node),
            Some(Frame::Seq { items, .. }) => items.push(node),
            Some(Frame::Map { entries, key, .. }) => match key.take() {
                Some(k) => {
                    // An empty value is positioned after the key's line.
                    let mut node = node;
                    if node.kind == NodeKind::Null {
                        node.line = k.line;
                    }
                    entries.push((k, node));
                }
                None => {
                    let (text, line) = match &node.kind {
                        NodeKind::Scalar { value, plain } => {
                            if *plain && value == "<<" {
                                return Err(
                                    self.err(node.line, "merge keys (<<) are not supported")
                                );
                            }
                            (value.clone(), node.line)
                        }
                        _ => {
                            return Err(self.err(node.line, "mapping keys must be strings"));
                        }
                    };
                    let dup = entries.iter().any(|(k, _)| {
                        matches!(&k.kind, NodeKind::Scalar { value, .. } if *value == text)
                    });
                    if dup {
                        return Err(self.err(line, format!("duplicate key `{text}`")));
                    }
                    *key = Some(node);
                }
            },
        }
        Ok(())
    }
}

/// Parses `text` into one tree, or says why the file is outside the subset.
pub fn parse(text: &str, file: &str) -> Result<Node, Error> {
    let mut parser = Parser::new_from_str(text);
    let mut b = Builder {
        file,
        stack: Vec::new(),
        root: None,
    };
    let mut documents = 0;
    loop {
        let (event, mark): (Event, Marker) = parser
            .next_token()
            .map_err(|e| Error::new(file, Some(e.marker().line()), "", e.info().to_string()))?;
        let line = mark.line();
        match event {
            Event::Nothing | Event::StreamStart | Event::DocumentEnd => {}
            Event::StreamEnd => break,
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return Err(b.err(line, "more than one document in the file"));
                }
            }
            Event::Alias(_) => return Err(b.err(line, "aliases are not supported")),
            Event::Scalar(value, style, anchor, tag) => {
                b.check_props(anchor, tag.is_some(), line)?;
                let plain = style == TScalarStyle::Plain;
                let kind = if plain && (value.is_empty() || value == "~" || value == "null") {
                    NodeKind::Null
                } else {
                    NodeKind::Scalar { value, plain }
                };
                b.push_value(Node { kind, line })?;
            }
            Event::SequenceStart(anchor, tag) => {
                b.check_props(anchor, tag.is_some(), line)?;
                b.stack.push(Frame::Seq {
                    line,
                    items: Vec::new(),
                });
            }
            Event::MappingStart(anchor, tag) => {
                b.check_props(anchor, tag.is_some(), line)?;
                b.stack.push(Frame::Map {
                    line,
                    entries: Vec::new(),
                    key: None,
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let node = match b.stack.pop() {
                    Some(Frame::Seq { line, items }) => Node {
                        kind: NodeKind::Seq(items),
                        line,
                    },
                    Some(Frame::Map { line, entries, .. }) => Node {
                        kind: NodeKind::Map(entries),
                        line,
                    },
                    None => return Err(b.err(line, "unbalanced collection end")),
                };
                b.push_value(node)?;
            }
        }
    }
    b.root
        .ok_or_else(|| Error::new(file, None, "", "empty file"))
}

/// Parses `text` and deserializes it into `T`.
pub fn from_str<T: DeserializeOwned>(text: &str, file: &str) -> Result<T, Error> {
    let node = parse(text, file)?;
    T::deserialize(De {
        node: &node,
        path: String::new(),
    })
    .map_err(|e| e.in_file(file))
}

// ---------------------------------------------------------- deserializing

/// A map whose entries keep the order of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ordered<V>(pub Vec<(String, V)>);

impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Ordered(Vec::new())
    }
}

impl<'de, V: de::Deserialize<'de>> de::Deserialize<'de> for Ordered<V> {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis<V>(std::marker::PhantomData<V>);
        impl<'de, V: de::Deserialize<'de>> Visitor<'de> for Vis<V> {
            type Value = Ordered<V>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a mapping")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, V>()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(Vis(std::marker::PhantomData))
    }
}

const SPANNED: &str = "$bree::Spanned";

/// A value together with the line it starts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub line: usize,
    pub value: T,
}

impl<'de, T: de::Deserialize<'de>> de::Deserialize<'de> for Spanned<T> {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis<T>(std::marker::PhantomData<T>);
        impl<'de, T: de::Deserialize<'de>> Visitor<'de> for Vis<T> {
            type Value = Spanned<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let line: u64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("missing line"))?;
                let value = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("missing value"))?;
                Ok(Spanned {
                    line: line as usize,
                    value,
                })
            }
        }
        d.deserialize_newtype_struct(SPANNED, Vis(std::marker::PhantomData))
    }
}

struct De<'a> {
    node: &'a Node,
    path: String,
}

fn join(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

fn describe(node: &Node) -> String {
    match &node.kind {
        NodeKind::Null => "null".to_string(),
        NodeKind::Scalar { value, plain: true } => format!("`{value}`"),
        NodeKind::Scalar {
            value,
            plain: false,
        } => format!("`\"{value}\"`"),
        NodeKind::Seq(_) => "a list".to_string(),
        NodeKind::Map(_) => "a mapping".to_string(),
    }
}

impl De<'_> {
    fn fail<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(Error::new(
            "",
            Some(self.node.line),
            &self.path,
            message.into(),
        ))
    }

    fn located<T>(&self, r: Result<T, Error>) -> Result<T, Error> {
        r.map_err(|e| e.at(self.node.line, &self.path))
    }

    fn plain_scalar(&self, expected: &str) -> Result<&str, Error> {
        match &self.node.kind {
            NodeKind::Scalar { value, plain: true } => Ok(value),
            _ => self.fail(format!(
                "expected {expected}, found {}",
                describe(self.node)
            )),
        }
    }

    fn seq_access<'n>(&self, items: &'n [Node]) -> SeqAcc<'n> {
        SeqAcc {
            items: items.iter().enumerate(),
            path: self.path.clone(),
        }
    }
}

struct SeqAcc<'a> {
    items: std::iter::Enumerate<std::slice::Iter<'a, Node>>,
    path: String,
}

impl<'de> SeqAccess<'de> for SeqAcc<'_> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        match self.items.next() {
            None => Ok(None),
            Some((i, node)) => seed
                .deserialize(De {
                    node,
                    path: format!("{}[{i}]", self.path),
                })
                .map(Some),
        }
    }
}

struct MapAcc<'a> {
    entries: std::slice::Iter<'a, (Node, Node)>,
    value: Option<(&'a Node, String)>,
    path: String,
}

impl<'de> MapAccess<'de> for MapAcc<'_> {
    type Error = Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        let Some((k, v)) = self.entries.next() else {
            return Ok(None);
        };
        let NodeKind::Scalar { value, .. } = &k.kind else {
            unreachable!("the parser only builds scalar keys");
        };
        let path = join(&self.path, value);
        self.value = Some((v, path.clone()));
        seed.deserialize(De { node: k, path }).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let (node, path) = self.value.take().expect("value follows key");
        seed.deserialize(De { node, path })
    }
}

struct SpannedAcc<'a> {
    line: Option<usize>,
    de: Option<De<'a>>,
}

impl<'de> SeqAccess<'de> for SpannedAcc<'_> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        if let Some(line) = self.line.take() {
            return seed
                .deserialize(IntoDeserializer::<Error>::into_deserializer(line as u64))
                .map(Some);
        }
        match self.de.take() {
            Some(de) => seed.deserialize(de).map(Some),
            None => Ok(None),
        }
    }
}

macro_rules! int_methods {
    ($($method:ident => $parse:ty, $visit:ident;)*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            let text = self.plain_scalar("an integer")?;
            match text.parse::<$parse>() {
                Ok(n) => self.located(visitor.$visit(n)),
                Err(_) => self.fail(format!("expected an integer, found {}", describe(self.node))),
            }
        }
    )*};
}

impl<'de, 'a> de::Deserializer<'de> for De<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let r = match &self.node.kind {
            NodeKind::Null => visitor.visit_unit(),
            NodeKind::Scalar { value, plain: true } if value == "true" => visitor.visit_bool(true),
            NodeKind::Scalar { value, plain: true } if value == "false" => {
                visitor.visit_bool(false)
            }
            NodeKind::Scalar { value, .. } => visitor.visit_str(value),
            NodeKind::Seq(items) => visitor.visit_seq(self.seq_access(items)),
            NodeKind::Map(entries) => visitor.visit_map(MapAcc {
                entries: entries.iter(),
                value: None,
                path: self.path.clone(),
            }),
        };
        self.located(r)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.plain_scalar("true or false")? {
            "true" => self.located(visitor.visit_bool(true)),
            "false" => self.located(visitor.visit_bool(false)),
            _ => self.fail(format!(
                "expected true or false, found {}",
                describe(self.node)
            )),
        }
    }

    int_methods! {
        deserialize_i8 => i64, visit_i64;
        deserialize_i16 => i64, visit_i64;
        deserialize_i32 => i64, visit_i64;
        deserialize_i64 => i64, visit_i64;
        deserialize_u8 => u64, visit_u64;
        deserialize_u16 => u64, visit_u64;
        deserialize_u32 => u64, visit_u64;
        deserialize_u64 => u64, visit_u64;
        deserialize_f32 => f64, visit_f64;
        deserialize_f64 => f64, visit_f64;
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.node.kind {
            NodeKind::Scalar { value, .. } => self.located(visitor.visit_str(value)),
            _ => self.fail(format!("expected a string, found {}", describe(self.node))),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.node.kind == NodeKind::Null {
            self.located(visitor.visit_none())
        } else {
            let node = self.node;
            let path = self.path.clone();
            let r = visitor.visit_some(self);
            r.map_err(|e| e.at(node.line, &path))
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.node.kind == NodeKind::Null {
            self.located(visitor.visit_unit())
        } else {
            self.fail(format!("expected nothing, found {}", describe(self.node)))
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if name == SPANNED {
            let line = self.node.line;
            let r = visitor.visit_seq(SpannedAcc {
                line: Some(line),
                de: Some(self),
            });
            return r;
        }
        let node = self.node;
        let path = self.path.clone();
        let r = visitor.visit_newtype_struct(self);
        r.map_err(|e| e.at(node.line, &path))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.node.kind {
            NodeKind::Seq(items) => {
                let r = visitor.visit_seq(self.seq_access(items));
                self.located(r)
            }
            _ => self.fail(format!("expected a list, found {}", describe(self.node))),
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.node.kind {
            NodeKind::Map(entries) => {
                let r = visitor.visit_map(MapAcc {
                    entries: entries.iter(),
                    value: None,
                    path: self.path.clone(),
                });
                self.located(r)
            }
            _ => self.fail(format!("expected a mapping, found {}", describe(self.node))),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match &self.node.kind {
            NodeKind::Scalar { value, .. } => {
                let r = visitor
                    .visit_enum(IntoDeserializer::<Error>::into_deserializer(value.as_str()));
                self.located(r)
            }
            _ => self.fail(format!("expected a string, found {}", describe(self.node))),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.located(visitor.visit_unit())
    }

    serde::forward_to_deserialize_any! {
        char bytes byte_buf unit_struct tuple tuple_struct
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields, rename_all = "kebab-case")]
    struct T {
        #[serde(default)]
        name: String,
        #[serde(default)]
        flag: bool,
        #[serde(default)]
        items: Vec<String>,
        #[serde(default)]
        inner: Option<Inner>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields, rename_all = "kebab-case")]
    struct Inner {
        #[serde(default)]
        kvm: bool,
        #[serde(default)]
        n: Option<u64>,
    }

    fn ok(text: &str) -> T {
        from_str(text, "t.yaml").unwrap()
    }

    fn err(text: &str) -> String {
        from_str::<T>(text, "t.yaml").unwrap_err().to_string()
    }

    #[test]
    fn yaml_11_booleans_are_strings_for_a_string_key() {
        for word in ["no", "yes", "on", "off", "No", "Y", "n"] {
            assert_eq!(ok(&format!("name: {word}\n")).name, word);
        }
    }

    #[test]
    fn yaml_11_booleans_are_errors_for_a_bool_key() {
        for word in ["no", "yes", "on", "off"] {
            assert_eq!(
                err(&format!("name: x\nflag: {word}\n")),
                format!("t.yaml:2: flag: expected true or false, found `{word}`")
            );
        }
        assert!(ok("flag: true\n").flag);
        assert!(!ok("flag: false\n").flag);
    }

    #[test]
    fn a_number_is_text_for_a_string_key() {
        assert_eq!(ok("name: 5.3\n").name, "5.3");
        assert_eq!(ok("items: [1, 2.50, true]\n").items, ["1", "2.50", "true"]);
    }

    #[test]
    fn a_bool_key_rejects_numbers_and_quoted_text() {
        assert_eq!(
            err("inner:\n  kvm: 1\n"),
            "t.yaml:2: inner.kvm: expected true or false, found `1`"
        );
        assert_eq!(
            err("flag: \"true\"\n"),
            "t.yaml:1: flag: expected true or false, found `\"true\"`"
        );
    }

    #[test]
    fn integers_parse_and_reject_text() {
        assert_eq!(ok("inner:\n  n: 7\n").inner.unwrap().n, Some(7));
        assert_eq!(
            err("inner:\n  n: seven\n"),
            "t.yaml:2: inner.n: expected an integer, found `seven`"
        );
        assert_eq!(ok("inner:\n  n: ~\n").inner.unwrap().n, None);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert_eq!(
            err("name: a\ncolour: red\n"),
            "t.yaml:2: colour: unknown field `colour`, expected one of `name`, `flag`, `items`, `inner`"
        );
        assert!(err("inner:\n  kvmm: true\n").starts_with("t.yaml:2: inner.kvmm: unknown field"));
    }

    #[test]
    fn duplicate_keys_are_errors_at_the_second() {
        assert_eq!(
            err("name: a\nflag: true\nname: b\n"),
            "t.yaml:3: duplicate key `name`"
        );
        assert!(err("inner:\n  n: 1\n  n: 2\n").contains("t.yaml:3: duplicate key `n`"));
    }

    #[test]
    fn two_documents_are_an_error() {
        assert!(err("name: a\n---\nname: b\n").contains("more than one document"));
    }

    #[test]
    fn empty_file_is_an_error() {
        assert_eq!(err(""), "t.yaml: empty file");
        assert_eq!(err("# only a comment\n"), "t.yaml: empty file");
    }

    #[test]
    fn anchors_are_errors() {
        assert!(err("name: &a x\n").contains("t.yaml:1: anchors are not supported"));
        assert!(err("items: &a [x]\n").contains("anchors are not supported"));
    }

    #[test]
    fn aliases_are_errors() {
        assert!(err("name: &a x\nitems: [*a]\n").contains("not supported"));
        assert!(err("name: *a\n").contains("t.yaml"));
    }

    #[test]
    fn merge_keys_are_errors() {
        assert_eq!(
            err("inner: {n: 1}\n<<: x\n"),
            "t.yaml:2: merge keys (<<) are not supported"
        );
    }

    #[test]
    fn tags_are_errors() {
        assert!(err("name: !!str x\n").contains("t.yaml:1: tags are not supported"));
        assert!(err("name: !foo x\n").contains("tags are not supported"));
        assert!(err("items: !!seq [x]\n").contains("tags are not supported"));
    }

    #[test]
    fn non_scalar_keys_are_errors() {
        assert!(err("? [a]\n: b\n").contains("mapping keys must be strings"));
    }

    #[test]
    fn wrong_shapes_name_the_key() {
        assert_eq!(
            err("items: x\n"),
            "t.yaml:1: items: expected a list, found `x`"
        );
        assert_eq!(
            err("inner: x\n"),
            "t.yaml:1: inner: expected a mapping, found `x`"
        );
        assert_eq!(
            err("items:\n  - a\n  - {b: c}\n"),
            "t.yaml:3: items[1]: expected a string, found a mapping"
        );
    }

    #[test]
    fn null_values() {
        assert_eq!(ok("inner:\n").inner, None);
        assert_eq!(ok("inner: null\n").inner, None);
        assert_eq!(ok("name: \"null\"\n").name, "null");
        assert_eq!(
            err("name:\n"),
            "t.yaml:1: name: expected a string, found null"
        );
    }

    #[test]
    fn syntax_errors_carry_the_line() {
        assert!(err("name: [a\n").starts_with("t.yaml:"));
    }

    #[test]
    fn spanned_gives_the_line() {
        #[derive(Deserialize)]
        struct S {
            v: Spanned<String>,
        }
        let s: S = from_str("a: 1\nv: x\n".replace("a: 1\n", "").as_str(), "t.yaml").unwrap();
        assert_eq!((s.v.line, s.v.value.as_str()), (1, "x"));
    }
}
