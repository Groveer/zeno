#![allow(dead_code)]
//! Skill registry — store loaded skills with category index.
//!
//! Provides:
//! - Exact and fuzzy name lookup
//! - Category-based listing (Tier 1)

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::skills::types::{CategoryInfo, SkillDefinition};

// ---------------------------------------------------------------------------
// SkillRegistry
// ---------------------------------------------------------------------------

/// Registry of loaded skills with category indexing.
#[derive(Clone)]
pub struct SkillRegistry {
    skills: Vec<SkillDefinition>,
    /// Category → CategoryInfo (description + skill names).
    categories: IndexMap<String, CategoryInfo>,
    /// Name → index for fast exact lookup.
    name_index: HashMap<String, usize>,
}

impl SkillRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
            categories: IndexMap::new(),
            name_index: HashMap::new(),
        }
    }

    /// Create a registry pre-populated from the given skills and categories.
    pub fn from_parts(
        skills: Vec<SkillDefinition>,
        categories: IndexMap<String, CategoryInfo>,
    ) -> Self {
        let mut registry = Self {
            skills,
            categories,
            name_index: HashMap::new(),
        };
        registry.rebuild_name_index();
        registry
    }

    /// Register a skill. If a skill with the same name exists, it is replaced.
    pub fn register(&mut self, skill: SkillDefinition) {
        // Remove existing skill with same name (last-writer-wins)
        self.skills.retain(|s| s.name != skill.name);

        let name = skill.name.clone();
        let category = skill.category.clone();

        // Update category info
        let cat_entry = self
            .categories
            .entry(category)
            .or_insert_with(|| CategoryInfo {
                description: String::new(),
                skill_names: Vec::new(),
            });
        if !cat_entry.skill_names.contains(&name) {
            cat_entry.skill_names.push(name.clone());
        }

        self.skills.push(skill);
        self.rebuild_name_index();
    }

    /// Get a skill by exact name match.
    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.name_index
            .get(name)
            .and_then(|&idx| self.skills.get(idx))
    }

    /// Get a skill by name (case-insensitive fallback).
    pub fn get_insensitive(&self, name: &str) -> Option<&SkillDefinition> {
        if let Some(skill) = self.get(name) {
            return Some(skill);
        }
        let lower = name.to_lowercase();
        self.name_index
            .get(&lower)
            .and_then(|&idx| self.skills.get(idx))
            .or_else(|| self.skills.iter().find(|s| s.name.to_lowercase() == lower))
    }

    /// Fuzzy match: exact → case-insensitive → substring.
    pub fn get_fuzzy(&self, name: &str) -> Option<&SkillDefinition> {
        self.get(name)
            .or_else(|| self.get_insensitive(name))
            .or_else(|| {
                let lower = name.to_lowercase();
                self.skills
                    .iter()
                    .find(|s| s.name.to_lowercase().contains(&lower))
            })
    }

    /// List all skills sorted by name (legacy, returns all).
    pub fn list_skills(&self) -> Vec<&SkillDefinition> {
        let mut skills: Vec<&SkillDefinition> = self.skills.iter().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Tier 0: Get all categories.
    pub fn categories(&self) -> &IndexMap<String, CategoryInfo> {
        &self.categories
    }

    /// Tier 1: List skills by category.
    pub fn list_by_category(&self, category: &str) -> Vec<&SkillDefinition> {
        let lower = category.to_lowercase();
        self.skills
            .iter()
            .filter(|s| s.category.to_lowercase() == lower)
            .collect()
    }

    /// Get all skills with `always_inject = true`.
    pub fn always_inject_skills(&self) -> Vec<&SkillDefinition> {
        self.skills.iter().filter(|s| s.always_inject).collect()
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Merge another registry into this one. Existing entries are overwritten.
    pub fn merge(&mut self, other: SkillRegistry) {
        for skill in other.skills {
            self.register(skill);
        }
        // Merge category descriptions (keep ours if both exist)
        for (cat, info) in other.categories {
            let mut merged = false;
            if let Some(existing) = self.categories.get_mut(&cat) {
                if existing.description.is_empty() && !info.description.is_empty() {
                    existing.description = info.description.clone();
                }
                for name in &info.skill_names {
                    if !existing.skill_names.contains(name) {
                        existing.skill_names.push(name.clone());
                    }
                }
                merged = true;
            }
            if !merged {
                self.categories.insert(cat, info);
            }
        }
    }

    fn rebuild_name_index(&mut self) {
        self.name_index.clear();
        for (idx, skill) in self.skills.iter().enumerate() {
            self.name_index.insert(skill.name.clone(), idx);
        }
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(name: &str, desc: &str, category: &str, always_inject: bool) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            description: desc.into(),
            content: format!("# {}", name),
            source: "test".into(),
            path: None,
            category: category.into(),
            always_inject,
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill(
            "tdd",
            "TDD workflow",
            "software-development",
            false,
        ));
        assert!(registry.get("tdd").is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_register_overwrite() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill(
            "tdd",
            "Version 1",
            "software-development",
            false,
        ));
        registry.register(make_skill(
            "tdd",
            "Version 2",
            "software-development",
            false,
        ));
        assert_eq!(registry.get("tdd").unwrap().description, "Version 2");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_get_insensitive() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill(
            "TDD-Guide",
            "Test skill",
            "software-development",
            false,
        ));
        assert!(registry.get_insensitive("tdd-guide").is_some());
    }

    #[test]
    fn test_get_fuzzy() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill(
            "tdd-workflow",
            "Test",
            "software-development",
            false,
        ));
        assert!(registry.get_fuzzy("tdd").is_some());
    }

    #[test]
    fn test_list_by_category() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill("tdd", "TDD", "devops", false));
        registry.register(make_skill("deploy", "Deploy", "devops", false));
        registry.register(make_skill("arxiv", "Search", "research", false));

        let devops = registry.list_by_category("devops");
        assert_eq!(devops.len(), 2);

        let research = registry.list_by_category("research");
        assert_eq!(research.len(), 1);
    }

    #[test]
    fn test_always_inject_skills() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill("tdd", "TDD", "dev", false));
        registry.register(make_skill("core", "Core guidelines", "builtin", true));
        registry.register(make_skill("deploy", "Deploy", "devops", false));

        let core = registry.always_inject_skills();
        assert_eq!(core.len(), 1);
        assert_eq!(core[0].name, "core");
    }

    #[test]
    fn test_merge() {
        let mut r1 = SkillRegistry::new();
        r1.register(make_skill("tdd", "TDD", "dev", false));

        let mut r2 = SkillRegistry::new();
        r2.register(make_skill("deploy", "Deploy", "devops", false));

        r1.merge(r2);
        assert_eq!(r1.len(), 2);
    }

    #[test]
    fn test_list_skills_sorted() {
        let mut registry = SkillRegistry::new();
        registry.register(make_skill("zebra", "Z", "general", false));
        registry.register(make_skill("alpha", "A", "general", false));
        registry.register(make_skill("middle", "M", "general", false));

        let names: Vec<&str> = registry
            .list_skills()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn test_from_parts() {
        let skills = vec![make_skill("tdd", "TDD", "dev", false)];
        let mut categories = IndexMap::new();
        categories.insert(
            "dev".into(),
            CategoryInfo {
                description: "Development".into(),
                skill_names: vec!["tdd".into()],
            },
        );
        let registry = SkillRegistry::from_parts(skills, categories);
        assert_eq!(registry.len(), 1);
        assert!(registry.get("tdd").is_some());
        assert_eq!(
            registry.categories().get("dev").unwrap().description,
            "Development"
        );
    }

    #[test]
    fn test_empty_registry() {
        let registry = SkillRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.categories().is_empty());
    }
}
