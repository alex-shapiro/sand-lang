//! `T(...)` constructions and patterns against a model of field binding:
//! positional arguments fill fields in declaration order, named arguments
//! fill the rest in any order, `..base` and defaults fill what remains, and
//! side effects happen in source order, then `..base`, then defaults in
//! declaration order.

use super::int;
use proptest::prelude::*;
use sandlang::{Config, Engine, Error, MemorySink};

fn run(src: &str) -> (Result<sandlang::Outcome, Error>, String) {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(engine.run_source("main.sand", src));
    (r, out.contents())
}

fn lit(v: i64) -> String {
    super::int_lit(v)
}

/// A struct of `int` fields, and one construction of it.
#[derive(Clone, Debug)]
struct Construction {
    /// The default of each field, if it has one.
    defaults: Vec<Option<i64>>,
    /// The value given for each field, if the construction gives it.
    given: Vec<i64>,
    /// The number of fields given positionally (a prefix of the fields).
    positional: usize,
    /// The fields given by name, in source order.
    named: Vec<usize>,
    /// The fields of `..base`, if the construction has one.
    base: Option<Vec<i64>>,
}

fn construction() -> impl Strategy<Value = Construction> {
    (1usize..6).prop_flat_map(|n| {
        (
            prop::collection::vec(prop::option::of(int()), n),
            prop::collection::vec(int(), n),
            0..=n,
            Just((0..n).collect::<Vec<usize>>()).prop_shuffle(),
            prop::collection::vec(any::<bool>(), n),
            prop::option::of(prop::collection::vec(int(), n)),
        )
            .prop_map(|(defaults, given, positional, order, named_mask, base)| {
                let named = order.into_iter().filter(|&i| i >= positional && named_mask[i]).collect();
                Construction { defaults, given, positional, named, base }
            })
    })
}

impl Construction {
    fn n(&self) -> usize {
        self.defaults.len()
    }

    fn is_given(&self, i: usize) -> bool {
        i < self.positional || self.named.contains(&i)
    }

    fn source(&self) -> String {
        let n = self.n();
        let fields: Vec<String> = (0..n)
            .map(|i| match self.defaults[i] {
                Some(d) => format!("f{i}: int = note({}, {})", 100 + i, lit(d)),
                None => format!("f{i}: int"),
            })
            .collect();
        let mut args: Vec<String> = (0..self.positional).map(|i| format!("note({i}, {})", lit(self.given[i]))).collect();
        args.extend(self.named.iter().map(|&i| format!("f{i}: note({i}, {})", lit(self.given[i]))));
        let base = match &self.base {
            Some(b) => {
                args.push("..base()".into());
                let all: Vec<String> = (0..n).map(|i| format!("f{i}: {}", lit(b[i]))).collect();
                format!("fn base() -> S {{\n    LOG.push(99)\n    BASE\n}}\nlet BASE = S({})\n", all.join(", "))
            }
            None => String::new(),
        };
        format!(
            "var LOG: [int] = []\nfn note(i: int, v: int) -> int {{\n    LOG.push(i)\n    v\n}}\nstruct S {{ {} }}\n{base}\
             LOG.clear()\nlet s = S({})\nprint(s)\nprint(LOG)\n",
            fields.join(", "),
            args.join(", ")
        )
    }

    /// The expected output, or the field that the construction misses.
    fn expected(&self) -> Result<String, usize> {
        let n = self.n();
        let mut values = Vec::new();
        let mut log: Vec<usize> = (0..self.positional).chain(self.named.iter().copied()).collect();
        if self.base.is_some() {
            log.push(99);
        }
        for i in 0..n {
            values.push(if self.is_given(i) {
                self.given[i]
            } else if let Some(b) = &self.base {
                b[i]
            } else if let Some(d) = self.defaults[i] {
                log.push(100 + i);
                d
            } else {
                return Err(i);
            });
        }
        let fields: Vec<String> = values.iter().enumerate().map(|(i, v)| format!("f{i}: {v}")).collect();
        let log: Vec<String> = log.iter().map(usize::to_string).collect();
        Ok(format!("S({})\n[{}]\n", fields.join(", "), log.join(", ")))
    }
}

/// A sub-pattern of a constructor pattern.
#[derive(Clone, Debug)]
enum Sub {
    Bind,
    /// A literal that matches if the field equals it.
    Lit(i64),
}

/// A value of a struct or a variant with `int` fields, and a pattern for it.
#[derive(Clone, Debug)]
struct Match {
    values: Vec<i64>,
    subs: Vec<Sub>,
    positional: usize,
    named: Vec<usize>,
    rest: bool,
    variant: bool,
}

fn matching() -> impl Strategy<Value = Match> {
    (1usize..6).prop_flat_map(|n| {
        let sub = prop_oneof![Just(Sub::Bind), (-2i64..2).prop_map(Sub::Lit)];
        (
            prop::collection::vec(-2i64..2, n),
            prop::collection::vec(sub, n),
            0..=n,
            Just((0..n).collect::<Vec<usize>>()).prop_shuffle(),
            prop::collection::vec(any::<bool>(), n),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(values, subs, positional, order, mask, rest, variant)| {
                let named = order.into_iter().filter(|&i| i >= positional && mask[i]).collect();
                Match { values, subs, positional, named, rest, variant }
            })
    })
}

impl Match {
    fn n(&self) -> usize {
        self.values.len()
    }

    fn sub(&self, i: usize) -> String {
        match self.subs[i] {
            Sub::Bind => format!("b{i}"),
            Sub::Lit(v) => lit(v),
        }
    }

    fn mentioned(&self) -> Vec<usize> {
        (0..self.positional).chain(self.named.iter().copied()).collect()
    }

    fn source(&self) -> String {
        let n = self.n();
        let fields: Vec<String> = (0..n).map(|i| format!("f{i}: int")).collect();
        let values: Vec<String> = self.values.iter().map(|&v| lit(v)).collect();
        let mut subs: Vec<String> = (0..self.positional).map(|i| self.sub(i)).collect();
        subs.extend(self.named.iter().map(|&i| format!("f{i}: {}", self.sub(i))));
        if self.rest {
            subs.push("..".into());
        }
        let binds: Vec<String> =
            self.mentioned().into_iter().filter(|&i| matches!(self.subs[i], Sub::Bind)).map(|i| format!("b{i}")).collect();
        let (decl, value, ctor) = if self.variant {
            (format!("enum E {{ V({}), W }}", fields.join(", ")), format!("E.V({})", values.join(", ")), "V")
        } else {
            (format!("struct S {{ {} }}", fields.join(", ")), format!("S({})", values.join(", ")), "S")
        };
        format!(
            "{decl}\nif let {ctor}({}) = {value} {{\n    print([{}])\n}} else {{\n    print(\"no\")\n}}\n",
            subs.join(", "),
            binds.join(", ")
        )
    }

    /// The expected output, or `None` if the pattern misses a field without `..`.
    fn expected(&self) -> Option<String> {
        let mentioned = self.mentioned();
        if !self.rest && mentioned.len() < self.n() {
            return None;
        }
        let matched = mentioned.iter().all(|&i| match self.subs[i] {
            Sub::Bind => true,
            Sub::Lit(v) => self.values[i] == v,
        });
        if !matched {
            return Some("no\n".into());
        }
        let binds: Vec<String> =
            mentioned.into_iter().filter(|&i| matches!(self.subs[i], Sub::Bind)).map(|i| self.values[i].to_string()).collect();
        Some(format!("[{}]\n", binds.join(", ")))
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn constructions_bind_fields_and_evaluate_in_order(c in construction()) {
        let src = c.source();
        let (r, out) = run(&src);
        match c.expected() {
            Ok(expected) => {
                prop_assert!(r.is_ok(), "{r:?}\n{src}");
                prop_assert_eq!(out, expected, "{}", src);
            }
            Err(missing) => {
                let e = r.unwrap_err().to_string();
                prop_assert!(e.contains(&format!("missing field `f{missing}`")), "{e}\n{src}");
            }
        }
    }

    #[test]
    fn constructor_patterns_match_fields_by_position_and_name(m in matching()) {
        let src = m.source();
        let (r, out) = run(&src);
        match m.expected() {
            Some(expected) => {
                prop_assert!(r.is_ok(), "{r:?}\n{src}");
                prop_assert_eq!(out, expected, "{}", src);
            }
            None => {
                let e = r.unwrap_err().to_string();
                prop_assert!(e.contains("add `..` to ignore the rest"), "{e}\n{src}");
            }
        }
    }
}
