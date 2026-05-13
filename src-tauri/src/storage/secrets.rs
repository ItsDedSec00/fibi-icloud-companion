use std::path::{Path, PathBuf};

/// Load env vars from a `.env` file located in the project root.
///
/// Walks upward from both the current working directory and the executable's
/// own directory looking for `.env`. The first one found is parsed with a
/// minimal `KEY=VALUE` reader and applied to the process environment
/// (overriding any pre-existing values).
///
/// We do the parsing ourselves rather than depending on `dotenvy` because we
/// hit a case where `dotenvy` silently ignored a line containing a multi-byte
/// UTF-8 character in the value (`Lüneburg`). The format is simple enough
/// that we don't need a library.
pub fn load_env_file() {
    let starts: Vec<PathBuf> = [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from)),
    ]
    .into_iter()
    .flatten()
    .collect();

    for start in starts {
        if let Some(found) = find_env_upwards(&start, 6) {
            apply_env_file(&found);
            return;
        }
    }
}

fn find_env_upwards(start: &Path, max_levels: usize) -> Option<PathBuf> {
    let mut cur = Some(start.to_path_buf());
    for _ in 0..=max_levels {
        let Some(dir) = cur else { break };
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent().map(PathBuf::from);
    }
    None
}

fn apply_env_file(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip optional `export ` prefix that some users put in .env files.
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        // Strip a single pair of surrounding quotes if present, otherwise
        // keep the value as-is (spaces and commas in the middle are fine).
        let value = value.trim();
        let value = strip_one_pair_of_quotes(value);
        std::env::set_var(key, value);
    }
}

fn strip_one_pair_of_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

pub fn get_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
