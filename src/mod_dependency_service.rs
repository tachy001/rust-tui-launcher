use crate::modrinth::{compatible, identifier, primary_file, version_rank, ModVersion, Target};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModRequest {
    pub project: Option<String>,
    pub version: Option<String>,
    pub expected_project: Option<String>,
}
pub(crate) trait ModCatalog {
    fn candidates(&self, request: &ModRequest) -> Result<Vec<ModVersion>>;
    fn check_task(&self) -> Result<()> {
        Ok(())
    }
}
pub(crate) trait ModPlanValidator {
    /// None accepts the plan; a reason rejects this combination; Err aborts on I/O or task failure.
    fn validate(&self, plan: &[ModVersion]) -> Result<Option<String>>;
}
impl<F: Fn(&ModRequest) -> Result<Vec<ModVersion>>> ModCatalog for F {
    fn candidates(&self, request: &ModRequest) -> Result<Vec<ModVersion>> {
        self(request)
    }
}

/// Resolves platform-declared dependencies before any installed Mod is replaced.
pub(crate) struct ModDependencyResolver<'a> {
    target: &'a Target,
    catalog: &'a dyn ModCatalog,
    cache: HashMap<ModRequest, Vec<ModVersion>>,
    attempts: usize,
    limit: usize,
    rejection: String,
    validator: Option<&'a dyn ModPlanValidator>,
    checked_plans: usize,
}
impl<'a> ModDependencyResolver<'a> {
    pub(crate) fn new(target: &'a Target, catalog: &'a dyn ModCatalog) -> Self {
        Self {
            target,
            catalog,
            cache: HashMap::new(),
            attempts: 0,
            limit: 4096,
            rejection: "没有满足全部必需依赖和不兼容约束的 Mod 版本组合".into(),
            validator: None,
            checked_plans: 0,
        }
    }
    pub(crate) fn with_validator(mut self, validator: &'a dyn ModPlanValidator) -> Self {
        self.validator = Some(validator);
        self
    }
    pub(crate) fn resolve(mut self, root: ModRequest) -> Result<Vec<ModVersion>> {
        match self.search(VecDeque::from([root]), BTreeMap::new())? {
            Some(plan) => Ok(plan),
            None => bail!("无法解析 Mod 必需依赖：{}", self.rejection),
        }
    }
    fn search(
        &mut self,
        mut pending: VecDeque<ModRequest>,
        planned: BTreeMap<String, ModVersion>,
    ) -> Result<Option<Vec<ModVersion>>> {
        self.catalog.check_task()?;
        self.attempts += 1;
        if self.attempts > self.limit {
            bail!("Mod 依赖搜索超过 {} 步，请固定版本后重试", self.limit);
        }
        if planned.len() > 256 || pending.len() > 2000 {
            bail!("Mod 必需依赖数量过多，拒绝继续递归");
        }
        let Some(request) = pending.pop_front() else {
            let plan = planned.into_values().collect::<Vec<_>>();
            if let Some(validator) = self.validator {
                self.checked_plans += 1;
                if self.checked_plans > 32 {
                    bail!("Mod 包内兼容性检查超过 32 组候选，请固定依赖版本后重试");
                }
                if let Some(reason) = validator.validate(&plan)? {
                    self.rejection = reason;
                    return Ok(None);
                }
                self.catalog.check_task()?;
            }
            return Ok(Some(plan));
        };
        if let Some(known) = request
            .version
            .as_ref()
            .and_then(|id| planned.values().find(|version| &version.id == id))
        {
            if request
                .expected_project
                .as_ref()
                .is_some_and(|project| project != &known.project_id)
            {
                self.rejection = "依赖版本与项目身份不匹配".into();
                return Ok(None);
            }
            return self.search(pending, planned);
        }
        if let Some(known) = request
            .expected_project
            .as_ref()
            .and_then(|project| planned.get(project))
        {
            if request.version.as_ref().is_some_and(|id| id != &known.id) {
                self.rejection = format!("项目 {} 的必需依赖存在固定版本冲突", known.project_id);
                return Ok(None);
            }
            return self.search(pending, planned);
        }
        let candidates = if let Some(cached) = self.cache.get(&request) {
            cached.clone()
        } else {
            let mut candidates = self.catalog.candidates(&request)?;
            for candidate in &candidates {
                identifier(&candidate.id)?;
                identifier(&candidate.project_id)?;
                if request
                    .version
                    .as_ref()
                    .is_some_and(|id| id != &candidate.id)
                    || request
                        .expected_project
                        .as_ref()
                        .is_some_and(|id| id != &candidate.project_id)
                {
                    bail!("Modrinth 依赖响应的项目或版本身份不一致");
                }
            }
            candidates.retain(|version| compatible(version, self.target));
            candidates.sort_by(|a, b| {
                version_rank(&a.version_type)
                    .cmp(&version_rank(&b.version_type))
                    .then_with(|| b.date_published.cmp(&a.date_published))
            });
            self.cache.insert(request.clone(), candidates.clone());
            candidates
        };
        if candidates.is_empty() {
            self.rejection = format!(
                "项目 {:?} 没有适用于 Minecraft {} / {} 的版本",
                request.project, self.target.minecraft, self.target.loader
            );
        }
        for candidate in candidates {
            self.catalog.check_task()?;
            if let Err(error) = primary_file(&candidate) {
                self.rejection = error.to_string();
                continue;
            }
            if let Some(known) = planned.get(&candidate.project_id) {
                if known.id == candidate.id {
                    if let Some(plan) = self.search(pending.clone(), planned.clone())? {
                        return Ok(Some(plan));
                    }
                } else {
                    self.rejection =
                        format!("项目 {} 的必需依赖存在版本冲突", candidate.project_id);
                }
                continue;
            }
            let mut next = planned.clone();
            next.insert(candidate.project_id.clone(), candidate.clone());
            if next.values().any(|version| {
                version.dependencies.iter().any(|dependency| {
                    dependency.dependency_type == "incompatible"
                        && next
                            .values()
                            .any(|other| dependency_matches(dependency, other))
                })
            }) {
                self.rejection = "待安装项目包含声明不兼容的 Mod".into();
                continue;
            }
            let mut requests = pending.clone();
            for dep in &candidate.dependencies {
                if dep.dependency_type != "required" {
                    continue;
                }
                if dep.project_id.is_none() && dep.version_id.is_none() {
                    bail!("必需依赖没有可解析的项目 / 版本 ID，请手动安装");
                }
                requests.push_back(ModRequest {
                    project: dep.project_id.clone(),
                    version: dep.version_id.clone(),
                    expected_project: dep.project_id.clone(),
                });
            }
            if let Some(plan) = self.search(requests, next)? {
                return Ok(Some(plan));
            }
        }
        Ok(None)
    }
}

pub(crate) fn dependency_matches(
    dependency: &crate::modrinth::Dependency,
    version: &ModVersion,
) -> bool {
    dependency_matches_ids(dependency, &version.project_id, &version.id)
}
pub(crate) fn dependency_matches_ids(
    dependency: &crate::modrinth::Dependency,
    project: &str,
    version: &str,
) -> bool {
    (dependency.project_id.is_some() || dependency.version_id.is_some())
        && dependency
            .project_id
            .as_ref()
            .is_none_or(|id| id == project)
        && dependency
            .version_id
            .as_ref()
            .is_none_or(|id| id == version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modrinth::{Dependency, ModFile};
    use std::cell::Cell;
    fn version(project: &str, id: &str, deps: Vec<Dependency>) -> ModVersion {
        ModVersion {
            id: id.into(),
            project_id: project.into(),
            name: project.into(),
            version_number: id.into(),
            version_type: "release".into(),
            game_versions: vec!["1.21.1".into()],
            loaders: vec!["fabric".into()],
            date_published: id.into(),
            dependencies: deps,
            files: vec![ModFile {
                url: "https://example.com/mod.jar".into(),
                filename: format!("{project}.jar"),
                primary: true,
                size: 3,
                hashes: HashMap::from([(
                    "sha1".into(),
                    "a9993e364706816aba3e25717850c26c9cd0d89d".into(),
                )]),
            }],
        }
    }
    fn dep(project: &str, id: Option<&str>, kind: &str) -> Dependency {
        Dependency {
            project_id: Some(project.into()),
            version_id: id.map(str::to_owned),
            dependency_type: kind.into(),
        }
    }
    fn target() -> Target {
        Target {
            minecraft: "1.21.1".into(),
            loader: "fabric".into(),
        }
    }
    fn root() -> ModRequest {
        ModRequest {
            project: Some("root".into()),
            version: None,
            expected_project: Some("root".into()),
        }
    }
    fn catalog<'a>(versions: &'a [ModVersion], calls: &'a Cell<usize>) -> impl ModCatalog + 'a {
        move |request: &ModRequest| {
            calls.set(calls.get() + 1);
            Ok(versions
                .iter()
                .filter(|version| {
                    request.version.as_ref().map_or_else(
                        || request.project.as_ref() == Some(&version.project_id),
                        |id| id == &version.id,
                    )
                })
                .cloned()
                .collect())
        }
    }
    #[test]
    fn backtracks_when_a_later_dependency_pins_an_older_candidate() {
        let versions = vec![
            version(
                "root",
                "root1",
                vec![dep("dep", None, "required"), dep("guard", None, "required")],
            ),
            version("dep", "v2", vec![]),
            version("dep", "v1", vec![]),
            version("guard", "guard1", vec![dep("dep", Some("v1"), "required")]),
        ];
        let calls = Cell::new(0);
        let source = catalog(&versions, &calls);
        let plan = ModDependencyResolver::new(&target(), &source)
            .resolve(root())
            .unwrap();
        assert_eq!(
            plan.iter()
                .find(|version| version.project_id == "dep")
                .unwrap()
                .id,
            "v1"
        );
        assert_eq!(calls.get(), 3);
    }
    #[test]
    fn version_specific_incompatibility_does_not_ban_an_entire_project() {
        let versions = vec![
            version(
                "root",
                "root1",
                vec![
                    dep("dep", None, "required"),
                    dep("dep", Some("v1"), "incompatible"),
                ],
            ),
            version("dep", "v2", vec![]),
            version("dep", "v1", vec![]),
        ];
        let calls = Cell::new(0);
        let source = catalog(&versions, &calls);
        let plan = ModDependencyResolver::new(&target(), &source)
            .resolve(root())
            .unwrap();
        assert!(plan.iter().any(|version| version.id == "v2"));
    }
    #[test]
    fn conflicting_pins_fail_and_cycles_terminate() {
        let calls = Cell::new(0);
        let versions = vec![
            version(
                "root",
                "root1",
                vec![
                    dep("dep", Some("v1"), "required"),
                    dep("dep", Some("v2"), "required"),
                ],
            ),
            version("dep", "v1", vec![]),
            version("dep", "v2", vec![]),
        ];
        assert!(
            ModDependencyResolver::new(&target(), &catalog(&versions, &calls))
                .resolve(root())
                .is_err()
        );
        let cycle = vec![
            version("root", "root1", vec![dep("dep", None, "required")]),
            version("dep", "v1", vec![dep("root", None, "required")]),
        ];
        assert_eq!(
            ModDependencyResolver::new(&target(), &catalog(&cycle, &calls))
                .resolve(root())
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn rejects_wrong_project_and_stops_at_a_bounded_search_limit() {
        let calls = Cell::new(0);
        let wrong = vec![version("wrong", "wrong1", vec![])];
        let source = |_request: &ModRequest| Ok(wrong.clone());
        assert!(ModDependencyResolver::new(&target(), &source)
            .resolve(root())
            .err()
            .unwrap()
            .to_string()
            .contains("身份不一致"));
        let versions = vec![
            version("root", "root1", vec![dep("dep", None, "required")]),
            version("dep", "v1", vec![]),
        ];
        let source = catalog(&versions, &calls);
        let target = target();
        let mut solver = ModDependencyResolver::new(&target, &source);
        solver.limit = 1;
        assert!(solver
            .resolve(root())
            .err()
            .unwrap()
            .to_string()
            .contains("搜索超过"));
    }
}
