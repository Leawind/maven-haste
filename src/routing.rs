use crate::config::RepositoryConfig;

#[derive(Clone, Debug)]
pub struct RouteEngine {
    repositories: Vec<RepositoryConfig>,
}

impl RouteEngine {
    pub fn new(repositories: Vec<RepositoryConfig>) -> Self {
        Self { repositories }
    }

    pub fn candidates(&self, relative_path: &str) -> Vec<&RepositoryConfig> {
        self.repositories
            .iter()
            .filter(|repository| participates(repository, relative_path))
            .collect()
    }

    pub fn repository(&self, name: &str) -> Option<&RepositoryConfig> {
        self.repositories
            .iter()
            .find(|repository| repository.name == name)
    }

    pub fn repositories(&self) -> &[RepositoryConfig] {
        &self.repositories
    }
}

fn participates(repository: &RepositoryConfig, path: &str) -> bool {
    for rule in &repository.rules {
        let (include, pattern) = match rule.strip_prefix('!') {
            Some(pattern) => (false, pattern),
            None => (true, rule.as_str()),
        };
        if glob_matches(pattern, path) {
            return include;
        }
    }
    true
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let width = value.len() + 1;
    let mut memo = vec![None; (pattern.len() + 1) * width];
    glob_matches_from(pattern, value, 0, 0, width, &mut memo)
}

fn glob_matches_from(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    width: usize,
    memo: &mut [Option<bool>],
) -> bool {
    let memo_index = pattern_index * width + value_index;
    if let Some(result) = memo[memo_index] {
        return result;
    }

    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == b'*' {
        let mut stars_end = pattern_index;
        while stars_end < pattern.len() && pattern[stars_end] == b'*' {
            stars_end += 1;
        }
        let recursive = stars_end - pattern_index >= 2;
        let after_stars = if recursive && pattern.get(stars_end) == Some(&b'/') {
            stars_end + 1
        } else {
            stars_end
        };
        glob_matches_from(pattern, value, after_stars, value_index, width, memo)
            || (value_index < value.len()
                && (recursive || value[value_index] != b'/')
                && glob_matches_from(pattern, value, pattern_index, value_index + 1, width, memo))
    } else {
        value_index < value.len()
            && pattern[pattern_index] == value[value_index]
            && glob_matches_from(
                pattern,
                value,
                pattern_index + 1,
                value_index + 1,
                width,
                memo,
            )
    };
    memo[memo_index] = Some(result);
    result
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    fn repository(name: &str, rules: &[&str]) -> RepositoryConfig {
        RepositoryConfig {
            name: name.into(),
            url: Url::parse("https://repo.example/").unwrap(),
            use_proxy: None,
            max_concurrency: None,
            rules: rules.iter().map(|rule| (*rule).into()).collect(),
        }
    }

    #[test]
    fn first_matching_rule_controls_participation() {
        let engine = RouteEngine::new(vec![
            repository("fabric", &["net/fabricmc/**", "!**"]),
            repository("fallback", &[]),
        ]);

        let fabric = engine.candidates("net/fabricmc/loader/1.0/loader-1.0.jar");
        assert_eq!(
            fabric
                .iter()
                .map(|repo| repo.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fabric", "fallback"]
        );

        let other = engine.candidates("com/example/demo/1.0/demo-1.0.jar");
        assert_eq!(
            other
                .iter()
                .map(|repo| repo.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fallback"]
        );
    }

    #[test]
    fn star_matches_within_a_single_path_segment() {
        assert!(glob_matches("net/fabricmc/*", "net/fabricmc/loader"));
        assert!(!glob_matches("net/fabricmc/*", "net/fabricmc/a/b/c.jar"));
    }

    #[test]
    fn globstar_matches_zero_or_more_path_segments() {
        assert!(glob_matches("net/fabricmc/**", "net/fabricmc/a/b/c.jar"));
        assert!(glob_matches("net/**/loader", "net/loader"));
        assert!(glob_matches("net/**/loader", "net/fabricmc/loader"));
        assert!(!glob_matches("net/fabricmc/**", "net/minecraft/a.jar"));
    }
}
