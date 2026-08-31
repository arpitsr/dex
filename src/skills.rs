use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::types::*;

pub(crate) fn parse_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(unquote(val.trim()));
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(unquote(val.trim()));
        }
    }
    let name = name?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        eprintln!(
            "[skills] ignoring invalid skill name '{}' in {}",
            name,
            path.display()
        );
        return None;
    }
    Some(Skill {
        name,
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
    })
}

/// Strip a single layer of surrounding quotes (single or double) from a YAML
/// scalar value, so `description: "Short description"` yields the bare value.
pub(crate) fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

pub(crate) fn discover_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").exists() {
                if let Some(skill) = parse_skill(&path.join("SKILL.md")) {
                    if seen.insert(skill.name.clone()) {
                        skills.push(skill);
                    } else {
                        eprintln!(
                            "[skills] ignoring duplicate skill '{}' at {}",
                            skill.name,
                            path.display()
                        );
                    }
                }
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub(crate) fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project-level skills
    if let Ok(cwd) = env::current_dir() {
        dirs.push(cwd.join(".dex/skills"));
        dirs.push(cwd.join(".agents/skills"));
    }
    // User-level skills, including the pre-rename `ak` location so existing
    // setups keep working.
    if let Some(cfg) = env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(&cfg).join("dex/skills"));
        dirs.push(PathBuf::from(cfg).join("ak/skills"));
    } else if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".config/dex/skills"));
        dirs.push(PathBuf::from(home).join(".config/ak/skills"));
    }
    dirs
}

pub(crate) fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let mut out = String::new();
    out.push_str("\n\nAvailable skills:\n");
    for skill in skills {
        out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    out.push_str("\nTo use a skill, type /skill:<name> or ask about it.\n");
    out
}
