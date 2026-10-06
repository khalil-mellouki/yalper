//! Measures secret redaction of one hook payload, as a hook process pays it: in a fresh process, so the
//! keyword automaton is built and the matching rules are compiled during the timed call.
//!
//! Each case runs in `YALPER_BENCH_CALLS` child processes (default 20) that each redact one payload twice
//! and report the first (cold) and second (warm) time. Run with `cargo bench --bench redact`.

use std::env;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yalper::redact::redact_json;

const DEFAULT_CALLS: usize = 20;
const KB: usize = 1000;

/// Words that contain no rule keyword, alone or followed by a space or a newline.
const PLAIN_WORDS: &[&str] = &[
    "fn", "add", "count", "total", "parse", "line", "value", "item", "index", "buffer", "length",
    "result", "return", "match", "loop", "while", "let", "mut", "if", "else", "for", "in", "pub",
    "struct", "impl", "the", "build", "run", "tool", "file", "path", "data", "list", "map", "read",
    "write", "error", "ok", "into", "from", "with", "after", "before", "until", "one", "two",
    "three", "four", "five", "six",
];

/// Ordinary code: it mentions common keywords (`key`, `token`, `api`, `auth`, `s.`, `ey`), so their rules
/// get compiled and run, but holds no secret.
const CODE_LINES: &[&str] = &[
    "    let key = cache.key_for(&request.path);\n",
    "    if let Some(token) = session.current_token() {\n",
    "        headers.insert(\"Authorization\", format!(\"Bearer {}\", token.value()));\n",
    "    }\n",
    "    let response = client.get(api_url).send()?;\n",
    "    // They keep the access log until the task is done.\n",
    "    for (index, item) in items.iter().enumerate() {\n",
    "        totals.push(item.amount * rates[index]);\n",
    "    }\n",
    "    Ok(Summary { count: items.len(), totals })\n",
];

fn plain_text(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 16);
    let mut word = 0;
    while text.len() < bytes {
        text.push_str(PLAIN_WORDS[word % PLAIN_WORDS.len()]);
        text.push(if word % 12 == 11 { '\n' } else { ' ' });
        word = word * 7 + 3;
        word %= 9973;
    }
    text.truncate(bytes);
    text
}

fn code_text(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 128);
    let mut line = 0;
    while text.len() < bytes {
        text.push_str(CODE_LINES[line % CODE_LINES.len()]);
        line += 1;
    }
    text.truncate(bytes);
    text
}

/// Code with two fake secrets near the start and the end. Built at run time so no token sits in the source.
fn code_with_secrets(bytes: usize) -> String {
    let github = format!("ghp_{}", "7Hq2Xc9LmB4vRt8Kp3Wz6Ny1Ds5Fg0JaE2ue");
    let aws = format!("AKIA{}", "Z7QK3M4XBV2PL6TC");
    let mut text = format!("export GITHUB_TOKEN={github}\n");
    text.push_str(&code_text(bytes.saturating_sub(100)));
    text.push_str(&format!("aws_access_key_id = {aws}\n"));
    text
}

/// A hostile string: every rule keyword, each followed by an assignment of a random-looking value, so every
/// rule is compiled and finds work.
fn every_keyword(bytes: usize) -> String {
    let rules = include_str!("../third_party/gitleaks/gitleaks.toml");
    let keywords: Vec<&str> = rules
        .split("keywords = [")
        .skip(1)
        .flat_map(|list| {
            list[..list.find(']').unwrap()]
                .split('"')
                .skip(1)
                .step_by(2)
        })
        .collect();
    assert!(keywords.len() > 200);
    let mut text = String::with_capacity(bytes + 128);
    let mut index = 0;
    while text.len() < bytes {
        let keyword = keywords[index % keywords.len()];
        text.push_str(&format!(
            "{keyword}_value = \"{keyword}Zq8Lx3Vn7Rb2Kt9Ws4Pd6Hf1Jm5Yc0Ag\"\n"
        ));
        index += 1;
    }
    text.truncate(bytes);
    text
}

/// A hostile string aimed at one slow rule: `pwd` and 0 to 7 random name characters, repeated. One run
/// of the password rules over it takes about half a second.
fn one_slow_rule(bytes: usize) -> String {
    const NAME: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_.-";
    let mut state: u64 = 7;
    let mut next = |bound: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % bound
    };
    let mut text = String::with_capacity(bytes + 16);
    while text.len() < bytes {
        text.push_str("pwd");
        for _ in 0..next(8) {
            text.push(char::from(NAME[next(NAME.len() as u64) as usize]));
        }
    }
    text
}

/// A `PostToolUse` payload whose tool output is `output`.
fn payload(output: String) -> Value {
    json!({
        "session_id": "4f2c8e1a-7b3d-4c9e-a1f0-2d6b8e5c3a71",
        "transcript_path": "/home/dev/.claude/projects/demo/4f2c8e1a-7b3d-4c9e-a1f0-2d6b8e5c3a71.jsonl",
        "cwd": "/home/dev/demo",
        "hook_event_name": "PostToolUse",
        "tool_name": "Read",
        "tool_input": { "file_path": "/home/dev/demo/src/main.rs" },
        "tool_response": { "type": "text", "file": { "filePath": "/home/dev/demo/src/main.rs", "content": output } },
        "tool_use_id": "toolu_01ABCDEFGH",
    })
}

struct Case {
    name: &'static str,
    build: fn() -> Value,
    /// Whether redaction must change the payload (checked in every child).
    changes: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "no keywords, 100 KB",
        build: || payload(plain_text(100 * KB)),
        changes: false,
    },
    Case {
        name: "code, 2 KB",
        build: || payload(code_text(2 * KB)),
        changes: false,
    },
    Case {
        name: "code, 30 KB",
        build: || payload(code_text(30 * KB)),
        changes: false,
    },
    Case {
        name: "code, 100 KB",
        build: || payload(code_text(100 * KB)),
        changes: false,
    },
    Case {
        name: "code with 2 secrets, 100 KB",
        build: || payload(code_with_secrets(100 * KB)),
        changes: true,
    },
    Case {
        name: "code with 2 secrets, 100 KB, JSON-escaped (contains \\\")",
        build: || payload(serde_json::to_string(&code_with_secrets(100 * KB)).unwrap()),
        changes: true,
    },
    Case {
        name: "code, 256 KiB",
        build: || payload(code_text(256 * 1024)),
        changes: false,
    },
    Case {
        name: "every keyword, 256 KiB (hostile)",
        build: || payload(every_keyword(256 * 1024)),
        changes: true,
    },
    Case {
        name: "one slow rule, 256 KiB (hostile)",
        build: || payload(one_slow_rule(256 * 1024)),
        changes: true,
    },
    Case {
        name: "every keyword, 5 strings of 256 KiB, cut to the payload budget (hostile)",
        build: || {
            let mut value = payload(String::new());
            value["tool_response"]["file"]["content"] = json!(vec![every_keyword(256 * 1024); 5]);
            value
        },
        changes: true,
    },
];

/// In a child process: redacts the case's payload twice and prints both times in nanoseconds.
fn child(case: &Case) {
    let original = (case.build)();
    let mut first = original.clone();
    let mut second = original.clone();
    let start = Instant::now();
    redact_json(&mut first);
    let cold = start.elapsed();
    let start = Instant::now();
    redact_json(&mut second);
    let warm = start.elapsed();
    assert_eq!(
        first != original,
        case.changes,
        "case {:?}: unexpected redaction result",
        case.name
    );
    println!("{} {}", cold.as_nanos(), warm.as_nanos());
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Median and p90 of `times`, in milliseconds.
fn summary(times: &mut [Duration]) -> String {
    times.sort();
    let calls = times.len();
    let median = if calls % 2 == 1 {
        millis(times[calls / 2])
    } else {
        (millis(times[calls / 2 - 1]) + millis(times[calls / 2])) / 2.0
    };
    let p90 = millis(times[(calls * 9).div_ceil(10) - 1]);
    format!("median {median:.2} ms, p90 {p90:.2} ms")
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if let Some(position) = args.iter().position(|arg| arg == "--child") {
        let index: usize = args[position + 1].parse().unwrap();
        child(&CASES[index]);
        return;
    }

    let calls = env::var("YALPER_BENCH_CALLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&calls: &usize| calls > 0)
        .unwrap_or(DEFAULT_CALLS);
    let exe = env::current_exe().unwrap();
    for (index, case) in CASES.iter().enumerate() {
        let (mut cold, mut warm) = (Vec::new(), Vec::new());
        for _ in 0..calls {
            let output = Command::new(&exe)
                .args(["--child", &index.to_string()])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let mut times = stdout
                .split_whitespace()
                .map(|nanos| Duration::from_nanos(nanos.parse().unwrap()));
            cold.push(times.next().unwrap());
            warm.push(times.next().unwrap());
        }
        println!(
            "redact ({os}, {calls} processes): {name}: first call in process {cold}; second call {warm}",
            os = env::consts::OS,
            name = case.name,
            cold = summary(&mut cold),
            warm = summary(&mut warm),
        );
    }
}
