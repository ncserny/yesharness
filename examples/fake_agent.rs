//! A stand-in agent for the tests that works the same on macOS, Linux and Windows (no shell scripts).
//! Reads the prompt on stdin, then answers with the first rule whose needle is in it:
//!
//!   fake_agent "needle=>reply" "other needle=>line one\nline two" "=>default reply"
//!
//! An empty needle always matches. A reply may start with steps, each ending in ';':
//!   sleep=SECONDS;   wait first
//!   touch=PATH;      create an empty file
//!   fail;            print the reply on stderr and exit with status 1
//! and may contain:
//!   {last:PREFIX}    the last word in the prompt that starts with PREFIX
//!   {file:PATH?A|B}  A if PATH exists, else B

use std::io::Read;

fn main() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    for rule in std::env::args().skip(1) {
        let Some((needle, reply)) = rule.split_once("=>") else { continue };
        if !input.contains(needle) {
            continue;
        }
        let mut reply = reply;
        let mut fail = false;
        loop {
            if let Some(rest) = reply.strip_prefix("fail;") {
                fail = true;
                reply = rest;
            } else if let Some((secs, rest)) = reply.strip_prefix("sleep=").and_then(|r| r.split_once(';')) {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs.parse().unwrap_or(0.0)));
                reply = rest;
            } else if let Some((path, rest)) = reply.strip_prefix("touch=").and_then(|r| r.split_once(';')) {
                let _ = std::fs::write(path, "");
                reply = rest;
            } else {
                break;
            }
        }
        let text = expand(&reply.replace("\\n", "\n"), &input);
        if fail {
            eprintln!("{text}");
            std::process::exit(1);
        }
        println!("{text}");
        return;
    }
}

fn expand(reply: &str, input: &str) -> String {
    let mut out = String::new();
    let mut rest = reply;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else { break };
        let token = &rest[i + 1..i + j];
        if let Some(prefix) = token.strip_prefix("last:") {
            out.push_str(input.split_whitespace().rfind(|w| w.starts_with(prefix)).unwrap_or(""));
        } else if let Some((path, choices)) = token.strip_prefix("file:").and_then(|t| t.split_once('?')) {
            let (yes, no) = choices.split_once('|').unwrap_or((choices, ""));
            out.push_str(if std::path::Path::new(path).exists() { yes } else { no });
        } else {
            out.push_str(&rest[i..i + j + 1]);
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out
}
