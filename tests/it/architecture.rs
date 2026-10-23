use std::collections::BTreeMap;
use std::path::Path;

use crate::support::walk;

const MODEL_MAY_NAME: [&str; 2] = ["model", "limits"];
const READERS_MAY_NOT_NAME: [&str; 8] =
    ["render", "converter", "pool", "config", "ledger", "identity", "ocr", "raster"];
const AMBIENT_STD: [&str; 5] = ["env", "fs", "net", "process", "thread"];
const LINT_LEVELS: [&str; 5] = ["allow", "expect", "warn", "deny", "forbid"];
const POOL: &str = "src/pool.rs";
const BYTE_ORDER_HOME: &str = "src/shared/binary.rs";

struct Source {
    path: String,
    code: String,
}

struct NonTest {
    code: String,
    test_modules: Vec<String>,
}

#[derive(Debug)]
struct Token<'a> {
    text: &'a str,
    line: usize,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn blank(ch: char) -> char {
    if ch == '\n' { '\n' } else { ' ' }
}

fn blank_block_comment(c: &[char], mut i: usize, out: &mut String) -> usize {
    let mut depth = 0;
    while i < c.len() {
        let opens = c[i] == '/' && c.get(i + 1) == Some(&'*');
        let closes = c[i] == '*' && c.get(i + 1) == Some(&'/');
        if opens || closes {
            depth += if opens { 1 } else { -1 };
            out.push_str("  ");
            i += 2;
            if depth == 0 {
                break;
            }
        } else {
            out.push(blank(c[i]));
            i += 1;
        }
    }
    i
}

/// Blanks comments and string, byte and char literals, keeping every newline.
fn strip(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let at = |i: usize| c.get(i).copied().unwrap_or('\0');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < c.len() {
        let raw_hashes = (at(i) == 'r'
            && (i == 0
                || !is_ident(c[i - 1])
                || (matches!(c[i - 1], 'b' | 'c') && (i < 2 || !is_ident(c[i - 2])))))
        .then(|| c[i + 1..].iter().take_while(|&&h| h == '#').count())
        .filter(|&hashes| at(i + 1 + hashes) == '"');
        if at(i) == '/' && at(i + 1) == '/' {
            while i < c.len() && c[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if at(i) == '/' && at(i + 1) == '*' {
            i = blank_block_comment(&c, i, &mut out);
        } else if let Some(hashes) = raw_hashes {
            let open = hashes + 2;
            out.extend(std::iter::repeat_n(' ', open));
            i += open;
            while i < c.len() && !(c[i] == '"' && (1..=hashes).all(|h| at(i + h) == '#')) {
                out.push(blank(c[i]));
                i += 1;
            }
            out.extend(std::iter::repeat_n(' ', hashes + 1));
            i += hashes + 1;
        } else if at(i) == '"' {
            out.push(' ');
            i += 1;
            while i < c.len() && c[i] != '"' {
                let width = if c[i] == '\\' { 2 } else { 1 };
                for ch in &c[i..(i + width).min(c.len())] {
                    out.push(blank(*ch));
                }
                i += width;
            }
            out.push(' ');
            i += 1;
        } else if at(i) == '\'' && (at(i + 1) == '\\' || at(i + 2) == '\'') {
            let mut end = if at(i + 1) == '\\' { i + 3 } else { i + 2 };
            while end < c.len() && c[end] != '\'' {
                end += 1;
            }
            out.extend(std::iter::repeat_n(' ', end + 1 - i));
            i = end + 1;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// The code a non-test build compiles: each item under a column-0
/// `#[cfg(test)]` is blanked, and the modules it declares out of line are named.
fn non_test(text: &str) -> NonTest {
    const MARK: &str = "#[cfg(test)]";
    let mut code: Vec<char> = strip(text).chars().collect();
    let mut test_modules = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let line_start = i == 0 || code[i - 1] == '\n';
        if !(line_start && code[i..].iter().take(MARK.len()).copied().eq(MARK.chars())) {
            i += 1;
            continue;
        }
        let mut end = i + MARK.len();
        let mut depth = 0_i32;
        while end < code.len() {
            let ch = code[end];
            end += 1;
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' => depth -= 1,
                '}' => {
                    depth -= 1;
                    if depth <= 0 {
                        break;
                    }
                }
                ';' if depth == 0 => break,
                _ => {}
            }
        }
        let item: String = code[i + MARK.len()..end].iter().collect();
        let words: Vec<Token<'_>> = tokens(&item);
        if let [.., module, name, semi] = words.as_slice()
            && module.text == "mod"
            && semi.text == ";"
        {
            test_modules.push(name.text.to_owned());
        }
        for ch in &mut code[i..end] {
            if *ch != '\n' {
                *ch = ' ';
            }
        }
        i = end;
    }
    NonTest { code: code.into_iter().collect(), test_modules }
}

fn tokens(code: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut line = 1;
    let mut chars = code.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c == '\n' {
            line += 1;
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        let mut end = start + c.len_utf8();
        let raw = c == 'r'
            && code
                .get(end..)
                .and_then(|s| s.strip_prefix('#'))
                .and_then(|s| s.chars().next())
                .is_some_and(is_ident);
        let text_start = if raw {
            chars.next();
            start + 2
        } else {
            start
        };
        if c == '\'' || is_ident(c) {
            while let Some(&(i, next)) = chars.peek().filter(|&&(_, next)| is_ident(next)) {
                end = i + next.len_utf8();
                chars.next();
            }
        } else if c == ':' && chars.next_if(|&(_, next)| next == ':').is_some() {
            end += 1;
        }
        out.push(Token { text: code.get(text_start..end).unwrap(), line });
    }
    out
}

fn skip_path(toks: &[Token<'_>], mut at: usize) -> usize {
    at += 1;
    loop {
        match toks.get(at).map(|t| t.text) {
            Some("::") if toks.get(at + 1).is_some_and(|t| t.text == "{") => {
                let mut depth = 0;
                at += 1;
                while let Some(t) = toks.get(at) {
                    at += 1;
                    match t.text {
                        "{" => depth += 1,
                        "}" => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("::") => at += 2,
            _ => return at,
        }
    }
}

/// Steps over the leading `crate`, `self` and `super` segments of the path at `at`, entered
/// `below` levels under its root: its first other segment, and the levels under the root
/// that segment is looked up in.
fn climb(toks: &[Token<'_>], mut at: usize, mut below: usize) -> (usize, usize) {
    while toks.get(at + 1).is_some_and(|t| t.text == "::") {
        below = match toks[at].text {
            "crate" => 0,
            "self" => below,
            "super" => below.saturating_sub(1),
            _ => break,
        };
        at += 2;
    }
    (at, below)
}

/// Collects the first segment of every path in the use tree at `at`, entered `below`
/// levels under its root, that names an item of the root itself.
fn heads<'t>(
    toks: &'t [Token<'t>],
    at: usize,
    below: usize,
    out: &mut Vec<&'t Token<'t>>,
) -> usize {
    let (at, below) = climb(toks, at, below);
    let Some(first) = toks.get(at) else { return at };
    if first.text != "{" {
        if below == 0 {
            out.push(first);
        }
        return skip_path(toks, at);
    }
    let mut at = at + 1;
    loop {
        match toks.get(at).map(|t| t.text) {
            None => return at,
            Some("}") => return at + 1,
            Some(",") => at += 1,
            Some("as") => at += 2,
            Some(_) => at = heads(toks, at, below, out),
        }
    }
}

fn module_path(path: &str) -> Vec<&str> {
    let in_crate = path.strip_prefix("python/src/").or(path.strip_prefix("src/")).unwrap_or(path);
    let mut segments: Vec<&str> = in_crate.split('/').collect();
    match segments.pop() {
        Some("lib.rs" | "mod.rs") | None => {}
        Some(file) => segments.push(file.trim_end_matches(".rs")),
    }
    segments
}

/// The first segment of every path that reaches the crate root through `crate`, `self` or `super`.
fn root_heads<'t>(path: &str, toks: &'t [Token<'t>]) -> Vec<&'t Token<'t>> {
    let depth = module_path(path).len();
    let mut out = Vec::new();
    let mut at = 0;
    while at < toks.len() {
        let anchored = matches!(toks[at].text, "crate" | "self" | "super")
            && (at == 0 || toks[at - 1].text != "::")
            && toks.get(at + 1).is_some_and(|t| t.text == "::");
        at = if anchored { heads(toks, at, depth, &mut out) } else { at + 1 };
    }
    out.retain(|t| t.text != "self");
    out
}

/// Maps each name the crate root re-exports to the module it comes from.
fn reexports(lib: &str) -> BTreeMap<String, String> {
    let code = non_test(lib).code;
    let toks = tokens(&code);
    let mut homes = BTreeMap::new();
    for (i, window) in toks.windows(4).enumerate() {
        if let [public, keyword, module, sep] = window
            && public.text == "pub"
            && keyword.text == "use"
            && sep.text == "::"
        {
            let mut last = None;
            for t in toks[i + 4..].iter().take_while(|t| t.text != ";") {
                match t.text {
                    "," | "}" => homes
                        .extend(last.take().map(|n: &str| (n.to_owned(), module.text.to_owned()))),
                    "{" | "::" | "as" => {}
                    name => last = Some(name),
                }
            }
            homes.extend(last.map(|n| (n.to_owned(), module.text.to_owned())));
        }
    }
    homes
}

fn home<'a>(head: &'a str, reexports: &'a BTreeMap<String, String>) -> &'a str {
    reexports.get(head).map_or(head, String::as_str)
}

fn model_names_only_model_and_limits(
    path: &str,
    code: &str,
    reexports: &BTreeMap<String, String>,
) -> Vec<String> {
    if !path.starts_with("src/model/") {
        return Vec::new();
    }
    let toks = tokens(code);
    root_heads(path, &toks)
        .into_iter()
        .filter(|t| !MODEL_MAY_NAME.contains(&home(t.text, reexports)))
        .map(|t| format!("{path}:{}: the model names crate::{}", t.line, t.text))
        .collect()
}

fn readers_name_no_engine(
    path: &str,
    code: &str,
    reexports: &BTreeMap<String, String>,
) -> Vec<String> {
    if !["src/formats/", "src/package/", "src/shared/"].iter().any(|dir| path.starts_with(dir)) {
        return Vec::new();
    }
    let toks = tokens(code);
    root_heads(path, &toks)
        .into_iter()
        .filter(|t| t.text == "*" || READERS_MAY_NOT_NAME.contains(&home(t.text, reexports)))
        .map(|t| format!("{path}:{}: a reader names crate::{}", t.line, t.text))
        .collect()
}

fn no_static_items(path: &str, code: &str) -> Vec<String> {
    if path == POOL {
        return Vec::new();
    }
    let toks = tokens(code);
    toks.iter()
        .enumerate()
        .filter(|&(i, t)| {
            t.text == "static"
                || (t.text == "thread_local" && toks.get(i + 1).is_some_and(|n| n.text == "!"))
        })
        .map(|(_, t)| format!("{path}:{}: a {} item", t.line, t.text))
        .collect()
}

fn no_inner_lint_levels(path: &str, code: &str) -> Vec<String> {
    let toks = tokens(code);
    let mut out = Vec::new();
    for (i, window) in toks.windows(4).enumerate() {
        let [hash, bang, open, name] = window else { continue };
        if (hash.text, bang.text, open.text) != ("#", "!", "[") {
            continue;
        }
        let names_level = LINT_LEVELS.contains(&name.text)
            || (name.text == "cfg_attr"
                && toks[i + 3..]
                    .iter()
                    .scan(1_i32, |depth, t| {
                        *depth += match t.text {
                            "[" => 1,
                            "]" => -1,
                            _ => 0,
                        };
                        (*depth > 0).then_some(t)
                    })
                    .any(|t| LINT_LEVELS.contains(&t.text)));
        if names_level {
            out.push(format!("{path}:{}: an inner lint level", hash.line));
        }
    }
    out
}

fn no_ambient_std(path: &str, code: &str) -> Vec<String> {
    if !path.starts_with("src/") || path == POOL {
        return Vec::new();
    }
    let toks = tokens(code);
    let mut found = Vec::new();
    for i in 0..toks.len() {
        if toks[i].text == "std" && toks.get(i + 1).is_some_and(|t| t.text == "::") {
            heads(&toks, i + 2, 0, &mut found);
        }
    }
    found
        .into_iter()
        .filter(|t| t.text == "*" || AMBIENT_STD.contains(&t.text))
        .map(|t| format!("{path}:{}: std::{}", t.line, t.text))
        .collect()
}

fn byte_order_lives_in_binary(path: &str, code: &str) -> Vec<String> {
    if path == BYTE_ORDER_HOME {
        return Vec::new();
    }
    tokens(code)
        .iter()
        .filter(|t| t.text == "from_le_bytes")
        .map(|t| format!("{path}:{}: from_le_bytes outside {BYTE_ORDER_HOME}", t.line))
        .collect()
}

fn release_checks_overflow_and_unwinds(manifest: &str) -> Vec<String> {
    let mut table = "";
    let mut overflow_checks = false;
    let mut out = Vec::new();
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            table = line;
            continue;
        }
        if table != "[profile.release]" && !table.starts_with("[profile.release.") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match (key.trim(), value.trim().trim_matches(['"', '\''])) {
            ("overflow-checks", "true") if table == "[profile.release]" => overflow_checks = true,
            ("overflow-checks", "false") => {
                out.push(format!("{table} turns overflow-checks off"));
            }
            ("panic", value) if value != "unwind" => {
                out.push(format!("{table} sets panic = {value}"));
            }
            _ => {}
        }
    }
    if !overflow_checks {
        out.push("the release profile does not set overflow-checks = true".to_owned());
    }
    out
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn module_dir(path: &str) -> &str {
    path.strip_suffix("/lib.rs")
        .or(path.strip_suffix("/mod.rs"))
        .or(path.strip_suffix(".rs"))
        .unwrap_or(path)
}

fn sources() -> Vec<Source> {
    let mut files = Vec::new();
    for dir in ["src", "python/src"] {
        walk(&root().join(dir), &mut files);
    }
    let mut sources = Vec::new();
    let mut test_only = Vec::new();
    for file in files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")) {
        let relative = file.strip_prefix(root()).unwrap();
        let path = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let parsed = non_test(&std::fs::read_to_string(file).unwrap());
        test_only
            .extend(parsed.test_modules.iter().map(|name| format!("{}/{name}", module_dir(&path))));
        sources.push(Source { path, code: parsed.code });
    }
    sources.retain(|s| {
        !test_only
            .iter()
            .any(|m| s.path == format!("{m}.rs") || s.path.starts_with(&format!("{m}/")))
    });
    assert!(sources.iter().any(|s| s.path == "src/model/mod.rs"));
    assert!(sources.iter().any(|s| s.path == "python/src/lib.rs"));
    assert!(sources.iter().all(|s| s.path != "src/shared/test_support.rs"));
    sources
}

fn crate_reexports() -> BTreeMap<String, String> {
    reexports(&std::fs::read_to_string(root().join("src/lib.rs")).unwrap())
}

fn assert_clean(rule: impl Fn(&Source) -> Vec<String>) {
    let found: Vec<String> = sources().iter().flat_map(rule).collect();
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn the_model_names_only_itself_and_the_limits() {
    let reexports = crate_reexports();
    assert_clean(|s| model_names_only_model_and_limits(&s.path, &s.code, &reexports));
}

#[test]
fn readers_name_no_engine_module() {
    let reexports = crate_reexports();
    assert_clean(|s| readers_name_no_engine(&s.path, &s.code, &reexports));
}

#[test]
fn only_the_pool_holds_static_items() {
    assert_clean(|s| no_static_items(&s.path, &s.code));
}

#[test]
fn lint_levels_live_in_the_manifest() {
    assert_clean(|s| no_inner_lint_levels(&s.path, &s.code));
}

#[test]
fn only_the_pool_names_ambient_std_modules() {
    assert_clean(|s| no_ambient_std(&s.path, &s.code));
}

#[test]
fn little_endian_decoding_lives_in_the_binary_module() {
    assert_clean(|s| byte_order_lives_in_binary(&s.path, &s.code));
}

#[test]
fn release_builds_check_overflow_and_unwind() {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let found = release_checks_overflow_and_unwinds(&manifest);
    assert!(found.is_empty(), "{}", found.join("\n"));
}

mod self_test {
    use super::*;

    const LIB: &str = "pub mod model;\nmod render;\n\
        pub use converter::{Converter, Input as In};\npub use limits::Limits;\n";

    fn reader_rule(path: &str, text: &str) -> Vec<String> {
        readers_name_no_engine(path, &non_test(text).code, &reexports(LIB))
    }

    #[test]
    fn reexports_map_to_their_modules() {
        let homes = reexports(LIB);
        let pairs: Vec<(&str, &str)> =
            homes.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(pairs, [("Converter", "converter"), ("In", "converter"), ("Limits", "limits")]);
    }

    #[test]
    fn a_column_0_test_item_is_dropped_and_the_code_after_it_kept() {
        let text = "#[cfg(test)]\npub(crate) fn helper(\n    a: u8,\n) {\n    let x = [1; 2];\n    crate::render::x();\n}\nuse crate::render::Sink;\n";
        assert_eq!(
            reader_rule("src/shared/a.rs", text),
            ["src/shared/a.rs:8: a reader names crate::render"]
        );
        let parsed = non_test(
            "#[cfg(test)]\npub(crate) mod helpers;\nmod real;\n#[cfg(test)]\nmod tests {\n}\n",
        );
        assert_eq!(parsed.test_modules, ["helpers"]);
        assert_eq!(
            parsed.code.lines().map(str::trim).collect::<Vec<_>>(),
            ["", "", "mod real;", "", "", ""]
        );
    }

    #[test]
    fn an_indented_cfg_test_drops_nothing() {
        let text =
            "struct S {\n    #[cfg(test)]\n    admitted: u64,\n    sink: crate::render::Sink,\n}\n";
        assert_eq!(
            reader_rule("src/formats/a.rs", text),
            ["src/formats/a.rs:4: a reader names crate::render"]
        );
    }

    #[test]
    fn readers_naming_an_engine_module_are_caught_however_they_name_it() {
        let cases = [
            ("use crate::render::Sink;", vec!["render"]),
            ("use crate::{ctx::ParseCtx, render::Sink};", vec!["render"]),
            (
                "use crate::{error::{ConvertError, PartName}, {ledger::Ledger}, model};",
                vec!["ledger"],
            ),
            ("fn f() { crate::pool::run_on_pool(); }", vec!["pool"]),
            ("use crate::Converter;", vec!["Converter"]),
            ("use crate::{In as Input, Limits};", vec!["In"]),
            ("use crate::*;", vec!["*"]),
            ("use super::super::config::Config;", vec!["config"]),
            ("let q = '\"'; let b = b'{'; use crate::render::Sink;", vec!["render"]),
            ("let s = br#\"\"\"#; use crate::render::Sink;", vec!["render"]),
            ("use crate::r#render::markdown;", vec!["render"]),
            ("use self::super::super::config::Config;", vec!["config"]),
            ("use self::super::super::{config, render, ctx};", vec!["config", "render"]),
            ("use super::{super::pool::Pool, xlsx};", vec!["pool"]),
            ("use self::{super::{super::ledger::Ledger}};", vec!["ledger"]),
            ("fn f() -> self::super::super::render::Sink {}", vec!["render"]),
            ("let w = crate::K as crate::config::Width;", vec!["config"]),
        ];
        for (text, names) in cases {
            let want: Vec<String> = names
                .iter()
                .map(|n| format!("src/formats/docx/mod.rs:1: a reader names crate::{n}"))
                .collect();
            assert_eq!(reader_rule("src/formats/docx/mod.rs", text), want, "{text}");
        }
        assert_eq!(
            reader_rule("src/formats/mod.rs", "use self::super::config::Config;"),
            ["src/formats/mod.rs:1: a reader names crate::config"]
        );
    }

    #[test]
    fn reader_text_naming_no_engine_module_is_clean() {
        for text in [
            "use crate::{ctx::ParseCtx, model::{Block, Inline}, Limits};",
            "let s = \"crate::render\"; // crate::render\n/* crate::{pool} /* nested */ crate::config */",
            "let s = r#\"crate::render \" crate::pool\"#; let c = '\"'; let q = '\\''; let b = b'{';",
            "fn f<'a>(x: &'a str) -> &'static str { x }",
            "use super::render::Sink;",
            "use self::super::render::Sink;",
            "use self::super::super::{ctx::ParseCtx, model::Block};",
            "use super::{super::shared::xml, xlsx::grid};",
            "pub(crate) fn render() {}",
        ] {
            assert_eq!(
                reader_rule("src/formats/docx/mod.rs", text),
                Vec::<String>::new(),
                "{text}"
            );
        }
        assert_eq!(
            reader_rule("src/render/markdown/mod.rs", "use crate::pool::Pool;"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_model_may_name_only_itself_and_the_limits() {
        let rule = |text: &str| {
            model_names_only_model_and_limits("src/model/block.rs", text, &reexports(LIB))
        };
        assert_eq!(
            rule(
                "use crate::{self as root, model::Inline, limits::Limit, Limits};\nuse super::Block;"
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            rule(
                "use crate::{model::Inline, diagnostics::Region};\nfn f() -> crate::Format {}\nuse super::super::Converter;"
            ),
            [
                "src/model/block.rs:1: the model names crate::diagnostics",
                "src/model/block.rs:2: the model names crate::Format",
                "src/model/block.rs:3: the model names crate::Converter",
            ]
        );
    }

    #[test]
    fn the_model_naming_another_module_is_caught_however_a_relative_path_climbs() {
        let cases = [
            ("src/model/mod.rs", "use self::super::config::Config;", vec!["config"]),
            ("src/model/mod.rs", "use self::super::{config, model};", vec!["config"]),
            (
                "src/model/block.rs",
                "use super::{super::diagnostics::Region, Inline};",
                vec!["diagnostics"],
            ),
            ("src/model/block.rs", "fn f() -> self::super::super::Converter {}", vec!["Converter"]),
            (
                "src/model/mod.rs",
                "use self::super::{model::Inline, limits::Limit, Limits};\nuse self::block::Block;",
                vec![],
            ),
            (
                "src/model/block.rs",
                "use self::super::Inline;\nuse super::{super::limits::Limit, block::Block};",
                vec![],
            ),
        ];
        for (path, text, names) in cases {
            let want: Vec<String> =
                names.iter().map(|n| format!("{path}:1: the model names crate::{n}")).collect();
            assert_eq!(
                model_names_only_model_and_limits(path, text, &reexports(LIB)),
                want,
                "{text}"
            );
        }
    }

    #[test]
    fn a_static_item_outside_the_pool_is_caught() {
        let text = "fn f(x: &'static str) {}\nstatic CACHE: u8 = 0;\nthread_local! { }\nlet s = \"static\";";
        assert_eq!(
            no_static_items("python/src/lib.rs", &non_test(text).code),
            ["python/src/lib.rs:2: a static item", "python/src/lib.rs:3: a thread_local item"]
        );
        assert_eq!(no_static_items("src/pool.rs", "static POOL: u8 = 0;"), Vec::<String>::new());
        assert_eq!(
            no_static_items("src/a.rs", "fn f() -> &'static str { \"\" }"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_inner_lint_level_is_caught() {
        for level in LINT_LEVELS {
            let text = format!("//! Crate.\n\n#![{level}(missing_docs)]\n");
            assert_eq!(
                no_inner_lint_levels("src/lib.rs", &non_test(&text).code),
                ["src/lib.rs:3: an inner lint level"]
            );
        }
        assert_eq!(
            no_inner_lint_levels("src/lib.rs", "#![cfg_attr(test, allow(dead_code))]"),
            ["src/lib.rs:1: an inner lint level"]
        );
        let clean = "#![doc = \"x\"]\n#![cfg_attr(docsrs, feature(doc_cfg))]\n#[allow(dead_code)]\nfn f() {}\n// #![allow(x)]";
        assert_eq!(no_inner_lint_levels("src/lib.rs", &non_test(clean).code), Vec::<String>::new());
    }

    #[test]
    fn little_endian_decoding_outside_the_binary_module_is_caught() {
        let text = "fn f(b: [u8; 2]) -> u16 {\n    u16::from_le_bytes(b)\n}\n// from_le_bytes\nlet s = \"from_le_bytes\";\n#[cfg(test)]\nmod tests {\n    fn g() { u32::from_le_bytes([0; 4]); }\n}\n";
        assert_eq!(
            byte_order_lives_in_binary("src/formats/doc/sprm.rs", &non_test(text).code),
            ["src/formats/doc/sprm.rs:2: from_le_bytes outside src/shared/binary.rs"]
        );
        assert_eq!(
            byte_order_lives_in_binary(BYTE_ORDER_HOME, &non_test(text).code),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_ambient_std_module_outside_the_pool_is_caught() {
        let text = "use std::fs;\nuse std::{io::Read, env::{var}};\nlet t = std::thread::spawn(f);\nuse std::*;\nuse std::r#fs::File;";
        assert_eq!(
            no_ambient_std("src/formats/a.rs", &non_test(text).code),
            [
                "src/formats/a.rs:1: std::fs",
                "src/formats/a.rs:2: std::env",
                "src/formats/a.rs:3: std::thread",
                "src/formats/a.rs:4: std::*",
                "src/formats/a.rs:5: std::fs",
            ]
        );
        let clean = "use std::{io::Read, panic::catch_unwind};\nlet s = \"std::fs\"; // std::env";
        assert_eq!(no_ambient_std("src/formats/a.rs", &non_test(clean).code), Vec::<String>::new());
        assert_eq!(no_ambient_std("src/pool.rs", "use std::thread;"), Vec::<String>::new());
        assert_eq!(no_ambient_std("python/src/lib.rs", "use std::fs;"), Vec::<String>::new());
    }

    #[test]
    fn a_release_profile_without_overflow_checks_or_with_abort_is_caught() {
        let cases: [(&str, &[&str]); 7] = [
            (
                "[profile.release]\nlto = \"thin\"\noverflow-checks = true # checked\npanic = \"unwind\"\n",
                &[],
            ),
            (
                "[profile.release]\npanic = \"abort\"\n[profile.dev]\noverflow-checks = true\n",
                &[
                    "[profile.release] sets panic = abort",
                    "the release profile does not set overflow-checks = true",
                ],
            ),
            (
                "[profile.release]\noverflow-checks = false\n",
                &[
                    "[profile.release] turns overflow-checks off",
                    "the release profile does not set overflow-checks = true",
                ],
            ),
            (
                "[profile.release]\noverflow-checks = true\npanic = 'abort'\n",
                &["[profile.release] sets panic = abort"],
            ),
            (
                "[profile.release]\noverflow-checks = true\n\
                 [profile.release.package.almiladoc]\noverflow-checks = false\n",
                &["[profile.release.package.almiladoc] turns overflow-checks off"],
            ),
            (
                "[profile.release.package.almiladoc]\noverflow-checks = true\n",
                &["the release profile does not set overflow-checks = true"],
            ),
            (
                "[profile.release]\noverflow-checks = true\npanic = 'unwind'\n\
                 [profile.release-lto]\npanic = \"abort\"\n",
                &[],
            ),
        ];
        for (manifest, expected) in cases {
            assert_eq!(release_checks_overflow_and_unwinds(manifest), expected, "{manifest}");
        }
    }
}
