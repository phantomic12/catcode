//! Configured skill discovery and `skill://name` resolution.
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillProvenance {
    Project,
    User,
}
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub provenance: SkillProvenance,
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("invalid skill URI")]
    InvalidUri,
    #[error("skill not found")]
    NotFound,
    #[error("skill escapes root")]
    Escape,
    #[error("skill root unavailable")]
    Unavailable,
    #[error("deprecated skill path")]
    Deprecated,
}

#[derive(Clone, Debug)]
pub struct SkillResolver {
    roots: Vec<(PathBuf, SkillProvenance)>,
}
impl SkillResolver {
    pub fn new(
        project_roots: impl IntoIterator<Item = PathBuf>,
        user_roots: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let mut roots = Vec::new();
        for p in project_roots {
            roots.push((p, SkillProvenance::Project));
        }
        for p in user_roots {
            roots.push((p, SkillProvenance::User));
        }
        Self { roots }
    }
    pub fn discover(&self) -> Vec<Skill> {
        let mut found = HashMap::<String, Skill>::new();
        for (root, prov) in &self.roots {
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            let Ok(rd) = fs::read_dir(&root) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                let Ok(c) = p.canonicalize() else { continue };
                if !c.starts_with(&root) {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_string();
                if valid_name(&name) && c.join("SKILL.md").is_file() {
                    found.entry(name.clone()).or_insert(Skill {
                        name,
                        path: c,
                        provenance: prov.clone(),
                    });
                }
            }
        }
        let mut v: Vec<_> = found.into_values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
    pub fn resolve(&self, uri: &str) -> Result<Skill, SkillError> {
        let name = uri.strip_prefix("skill://").ok_or(SkillError::InvalidUri)?;
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
            || name.contains('%')
            || name == "."
            || name == ".."
            || name.split('/').any(|x| x == "..")
        {
            return Err(SkillError::InvalidUri);
        };
        self.discover()
            .into_iter()
            .find(|s| s.name == name)
            .ok_or(SkillError::NotFound)
    }
    pub fn read(&self, uri: &str) -> Result<String, SkillError> {
        let s = self.resolve(uri)?;
        fs::read_to_string(s.path.join("SKILL.md")).map_err(|_| SkillError::Unavailable)
    }
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && s != "."
        && s != ".."
}
pub fn configured(project: &Path, user: Option<&Path>) -> SkillResolver {
    let project_root = project.join(".catalyst-code/skills");
    let user_roots = user
        .into_iter()
        .map(|u| u.join(".config/catalyst-code/skills"));
    SkillResolver::new([project_root], user_roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn td() -> PathBuf {
        std::env::temp_dir().join(format!(
            "skill-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn precedence_and_uri() {
        let a = td();
        let b = td();
        fs::create_dir_all(a.join("x")).unwrap();
        fs::create_dir_all(b.join("x")).unwrap();
        fs::write(a.join("x/SKILL.md"), "project").unwrap();
        fs::write(b.join("x/SKILL.md"), "user").unwrap();
        let r = SkillResolver::new([a.clone()], [b.clone()]);
        assert_eq!(r.read("skill://x").unwrap(), "project");
        assert!(matches!(
            r.resolve("skill://../x"),
            Err(SkillError::InvalidUri)
        ));
        let _ = fs::remove_dir_all(a);
        let _ = fs::remove_dir_all(b);
    }
}
