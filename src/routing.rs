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
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut star_value) = (None, 0);

    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    fn repository(name: &str, rules: &[&str]) -> RepositoryConfig {
        RepositoryConfig {
            name: name.into(),
            url: Url::parse("https://repo.example/").unwrap(),
            rules: rules.iter().map(|rule| (*rule).into()).collect(),
        }
    }

    #[test]
    fn first_matching_rule_controls_participation() {
        let engine = RouteEngine::new(vec![
            repository("fabric", &["net/fabricmc/*", "!*"]),
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
    fn star_matches_across_path_separators() {
        assert!(glob_matches("net/fabricmc/*", "net/fabricmc/a/b/c.jar"));
        assert!(!glob_matches("net/fabricmc/*", "net/minecraft/a.jar"));
    }
}
