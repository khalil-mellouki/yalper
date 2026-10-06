//! Turns the vendored gitleaks rules into a static Rust table for `src/redact.rs`, so no TOML is parsed
//! while a hook runs.
//!
//! The rules file is `config/gitleaks.toml` from gitleaks v8.30.1 (https://github.com/gitleaks/gitleaks),
//! copied unchanged into `third_party/gitleaks/` next to its MIT license. Only what redaction uses is kept:
//! each rule's id, regex, secret group, entropy threshold, and keywords. Rules without a regex (they match
//! file paths only) and all allowlists are left out.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const RULES_FILE: &str = "third_party/gitleaks/gitleaks.toml";
const OUTPUT_FILE: &str = "gitleaks_rules.rs";

fn main() {
    println!("cargo::rerun-if-changed={RULES_FILE}");
    let text = fs::read_to_string(RULES_FILE)
        .unwrap_or_else(|error| panic!("reading {RULES_FILE}: {error}"));
    let config: toml::Table = text
        .parse()
        .unwrap_or_else(|error| panic!("parsing {RULES_FILE}: {error}"));
    let rules = config
        .get("rules")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("{RULES_FILE} has no [[rules]]"));

    let mut table = String::new();
    // Lowercase keyword to the indexes of the rules that list it.
    let mut keywords: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut count = 0;
    for rule in rules {
        let Some(regex) = rule.get("regex") else {
            continue;
        };
        let regex = regex.as_str().expect("regex is a string");
        let id = rule
            .get("id")
            .and_then(toml::Value::as_str)
            .expect("every rule has an id");
        let secret_group = rule.get("secretGroup").map_or(0, |group| {
            group.as_integer().expect("secretGroup is an integer")
        });
        let entropy = rule.get("entropy").map_or(0.0, |entropy| {
            entropy
                .as_float()
                .or_else(|| entropy.as_integer().map(|value| value as f64))
                .expect("entropy is a number")
        });
        let rule_keywords = rule
            .get("keywords")
            .and_then(toml::Value::as_array)
            .filter(|list| !list.is_empty())
            // Redaction only runs a rule when one of its keywords occurs. A rule without keywords would have
            // to run on every string, so it needs a decision before it is added.
            .unwrap_or_else(|| panic!("rule {id} has a regex but no keywords"));
        for keyword in rule_keywords {
            let keyword = keyword
                .as_str()
                .expect("keywords are strings")
                .to_ascii_lowercase();
            keywords.entry(keyword).or_default().push(count);
        }
        writeln!(
            table,
            "    Rule {{ id: {id:?}, regex: {regex:?}, secret_group: {secret_group}, entropy: {entropy:?} }},"
        )
        .unwrap();
        count += 1;
    }

    let mut code = format!(
        "const RULE_COUNT: usize = {count};\n\nstatic RULES: [Rule; RULE_COUNT] = [\n{table}];\n\n"
    );
    writeln!(code, "static KEYWORDS: [&str; {}] = [", keywords.len()).unwrap();
    for keyword in keywords.keys() {
        writeln!(code, "    {keyword:?},").unwrap();
    }
    writeln!(
        code,
        "];\n\nstatic KEYWORD_RULES: [&[u16]; {}] = [",
        keywords.len()
    )
    .unwrap();
    for indexes in keywords.values() {
        writeln!(code, "    &{indexes:?},").unwrap();
    }
    code.push_str("];\n");

    let out_dir = env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");
    fs::write(Path::new(&out_dir).join(OUTPUT_FILE), code).expect("writing the rules table");
}
