use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_QUERIES: usize = 10;

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    #[serde(default)]
    queries: Vec<String>,
}

fn path() -> PathBuf {
    if let Ok(p) = std::env::var("HIBIAS_RECENT_FILE") {
        return PathBuf::from(p);
    }
    PathBuf::from("hibias-recent.json")
}

pub fn load_queries() -> Vec<String> {
    let p = path();
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    serde_json::from_str::<Stored>(&s)
        .map(|s| s.queries)
        .unwrap_or_default()
}

pub fn save_queries(queries: &[String]) {
    let p = path();
    let s = Stored {
        queries: queries.to_vec(),
    };
    let Ok(text) = serde_json::to_string_pretty(&s) else {
        return;
    };
    let _ = std::fs::write(&p, text);
}

/// Push `q` to the front of `queries`, deduping case-insensitively and
/// capping to `MAX_QUERIES`. No-op for whitespace-only input.
pub fn push_query(queries: &mut Vec<String>, q: &str) {
    let trimmed = q.trim();
    if trimmed.is_empty() {
        return;
    }
    let lower = trimmed.to_lowercase();
    queries.retain(|x| x.to_lowercase() != lower);
    queries.insert(0, trimmed.to_string());
    if queries.len() > MAX_QUERIES {
        queries.truncate(MAX_QUERIES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_dedups_case_insensitively_and_moves_to_front() {
        let mut q = vec!["beatles".into(), "weezer".into()];
        push_query(&mut q, "BEATLES");
        assert_eq!(q, vec!["BEATLES", "weezer"]);
    }

    #[test]
    fn push_caps_at_max() {
        let mut q: Vec<String> = (0..MAX_QUERIES).map(|i| format!("q{i}")).collect();
        push_query(&mut q, "new");
        assert_eq!(q.len(), MAX_QUERIES);
        assert_eq!(q[0], "new");
    }

    #[test]
    fn push_skips_whitespace_only() {
        let mut q = vec!["a".into()];
        push_query(&mut q, "   ");
        assert_eq!(q, vec!["a"]);
    }

    #[test]
    fn push_trims_whitespace() {
        let mut q = vec![];
        push_query(&mut q, "  hello  ");
        assert_eq!(q, vec!["hello"]);
    }
}
