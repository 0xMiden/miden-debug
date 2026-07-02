use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use miden_assembly::SourceManager;
use miden_assembly_syntax::{
    Library,
    diagnostics::{IntoDiagnostic, Report},
};
use miden_core::{Word, serde::Deserializable};
use miden_mast_package::{Dependency, Package, TargetType};
use miden_package_registry::{
    PackageId, PackageProvider, PackageRecord, PackageRegistry, PackageVersions, Version,
    VersionRequirement,
};

use crate::{
    DebuggerConfig,
    linker::{LibraryKind, load_local_package_from_path, load_package_from_path},
};

pub(crate) fn load_packages(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
    root_package: Option<&Package>,
    log_target: &'static str,
) -> Result<Vec<Arc<Package>>, Report> {
    let mut registry = LocalPackageRegistry::default();
    let mut packages = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(root_package) = root_package {
        registry.install_package(Arc::new(root_package.clone()));
    }

    for link_library in config.link_libraries.iter() {
        log::debug!(target: log_target, "loading link library {}", link_library.name());
        match link_library.kind {
            LibraryKind::Masp => {
                let package = link_library.load_package(config)?;
                registry.install_package(package.clone());
                push_package(&mut packages, &mut seen, package);
            }
            LibraryKind::Masm => {
                let lib = link_library.load(config, source_manager.clone())?;
                push_package(
                    &mut packages,
                    &mut seen,
                    package_from_library(link_library.name.as_ref(), lib),
                );
            }
        }
    }

    registry.discover(config, log_target)?;
    if let Some(root_package) = root_package {
        for package in registry.resolve_dependency_packages(root_package)? {
            push_package(&mut packages, &mut seen, package);
        }
    } else {
        for package in registry.library_packages() {
            push_package(&mut packages, &mut seen, package.clone());
        }

        if let Some(toolchain_dir) = config.toolchain_dir() {
            for package in load_legacy_packages(&toolchain_dir, log_target)? {
                push_package(&mut packages, &mut seen, package);
            }
        }
    }

    Ok(packages)
}

fn push_package(
    packages: &mut Vec<Arc<Package>>,
    seen: &mut BTreeSet<Word>,
    package: Arc<Package>,
) {
    if seen.insert(package.digest()) {
        packages.push(package);
    }
}

/// Wrap a bare library in a synthetic library package, so the rest of the loading pipeline can
/// work strictly in terms of packages. Used for source-form MASM link libraries and legacy
/// `.masl` artifacts, which carry no package metadata of their own.
fn package_from_library(name: &str, library: Arc<Library>) -> Arc<Package> {
    Package::from_library(
        name.into(),
        "0.0.0".parse().expect("valid version"),
        TargetType::Library,
        library,
        [],
    )
    .into()
}

#[derive(Default)]
struct LocalPackageRegistry {
    packages: BTreeMap<PackageId, PackageVersions>,
    artifacts: BTreeMap<(PackageId, Version), Arc<Package>>,
    artifacts_by_digest: BTreeMap<(PackageId, Word), Arc<Package>>,
}

impl LocalPackageRegistry {
    fn install_package(&mut self, package: Arc<Package>) {
        let version = Version::new(package.version.clone(), package.digest());
        let dependencies = package.manifest.dependencies().map(|dep| {
            (
                dep.name.clone(),
                VersionRequirement::Exact(Version::new(dep.version.clone(), dep.digest)),
            )
        });
        let record = PackageRecord::new(version.clone(), dependencies);

        self.packages
            .entry(package.name.clone())
            .or_default()
            .entry(package.version.clone())
            .or_insert(record);
        self.artifacts.insert((package.name.clone(), version.clone()), package.clone());
        self.artifacts_by_digest
            .insert((package.name.clone(), package.digest()), package);
    }

    fn discover(
        &mut self,
        config: &DebuggerConfig,
        log_target: &'static str,
    ) -> Result<(), Report> {
        if let Some(toolchain_dir) = config.toolchain_dir() {
            self.install_packages_from_dir(&toolchain_dir.join("lib"), log_target, false)?;
            self.install_packages_from_dir(&toolchain_dir, log_target, false)?;
        }

        for search_path in &config.search_path {
            self.install_packages_from_dir(search_path, log_target, false)?;
        }

        if let Some(working_dir) = &config.working_dir {
            self.install_packages_from_dir(working_dir, log_target, true)?;
        }

        for target_dir in cargo_target_dirs(config) {
            self.install_packages_from_cargo_target(&target_dir, log_target)?;
        }

        Ok(())
    }

    fn install_packages_from_cargo_target(
        &mut self,
        target_dir: &Path,
        log_target: &'static str,
    ) -> Result<(), Report> {
        let entries = match std::fs::read_dir(target_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };

        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let profile_dir = entry.path();
            if !profile_dir.is_dir() {
                continue;
            }

            let build_dir = profile_dir.join("build");
            let build_entries = match std::fs::read_dir(&build_dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for build_entry in build_entries {
                let Ok(build_entry) = build_entry else {
                    continue;
                };
                self.install_packages_from_dir(
                    &build_entry.path().join("out").join("assets"),
                    log_target,
                    true,
                )?;
            }
        }

        self.install_profile_packages(target_dir, "miden", log_target, true)?;
        self.install_profile_packages(target_dir, "midenc/miden", log_target, true)?;

        Ok(())
    }

    fn install_profile_packages(
        &mut self,
        target_dir: &Path,
        relative_dir: &str,
        log_target: &'static str,
        local_only: bool,
    ) -> Result<(), Report> {
        let root = target_dir.join(relative_dir);
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };

        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.is_dir() {
                self.install_packages_from_dir(&path, log_target, local_only)?;
            }
        }

        Ok(())
    }

    fn install_packages_from_dir(
        &mut self,
        dir: &Path,
        log_target: &'static str,
        local_only: bool,
    ) -> Result<(), Report> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };

        for entry in entries {
            let entry = entry.into_diagnostic()?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("masp")) {
                continue;
            }
            log::debug!(target: log_target, "indexing package {}", path.display());
            let package = if local_only {
                load_local_package_from_path(&path)
            } else {
                load_package_from_path(&path)
            };
            match package {
                Ok(package) => self.install_package(package),
                Err(err) => {
                    log::debug!(
                        target: log_target,
                        "skipping package artifact {}: {err}",
                        path.display()
                    );
                }
            }
        }

        Ok(())
    }

    fn library_packages(&self) -> impl Iterator<Item = &Arc<Package>> {
        self.artifacts.values().filter(|package| package.is_library())
    }

    fn resolve_dependency_packages(&self, package: &Package) -> Result<Vec<Arc<Package>>, Report> {
        let mut packages = Vec::new();
        let mut visited = BTreeSet::new();

        for dependency in package.manifest.dependencies() {
            self.resolve_dependency(dependency, &mut visited, &mut packages)?;
        }

        Ok(packages)
    }

    fn resolve_dependency(
        &self,
        dependency: &Dependency,
        visited: &mut BTreeSet<(PackageId, Word)>,
        packages: &mut Vec<Arc<Package>>,
    ) -> Result<(), Report> {
        if !visited.insert((dependency.name.clone(), dependency.digest)) {
            return Ok(());
        }

        let version = Version::new(dependency.version.clone(), dependency.digest);
        let package = self.load_package(&dependency.name, &version).map_err(|_| {
            Report::msg(format!(
                "missing dependency '{}' (kind {}, version {}, digest {}). Build the package with \
                 `cargo-miden miden build` or load it explicitly with `-l <path-to-{}.masp>`.",
                dependency.name,
                dependency.kind,
                dependency.version,
                dependency.digest,
                dependency.name
            ))
        })?;

        for child in package.manifest.dependencies() {
            self.resolve_dependency(child, visited, packages)?;
        }

        if package.is_library() {
            packages.push(package);
        }

        Ok(())
    }
}

impl PackageRegistry for LocalPackageRegistry {
    fn available_versions(&self, package: &PackageId) -> Option<&PackageVersions> {
        self.packages.get(package)
    }
}

impl PackageProvider for LocalPackageRegistry {
    fn load_package(&self, package: &PackageId, version: &Version) -> Result<Arc<Package>, Report> {
        if let Some(digest) = version.digest
            && let Some(package) = self.artifacts_by_digest.get(&(package.clone(), digest))
        {
            return Ok(package.clone());
        }

        self.artifacts
            .get(&(package.clone(), version.clone()))
            .cloned()
            .ok_or_else(|| Report::msg(format!("cannot load package {package}@{version}")))
    }
}

fn cargo_target_dirs(config: &DebuggerConfig) -> BTreeSet<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.insert(config.working_dir().into_owned());

    if let Some(crate::InputFile::Real(path)) = config.input.as_ref() {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            config.working_dir().join(path)
        };
        if let Some(parent) = path.parent() {
            roots.insert(parent.to_path_buf());
        }
    }

    let mut targets = BTreeSet::new();
    for root in roots {
        for ancestor in root.ancestors() {
            let target = ancestor.join("target");
            if target.is_dir() {
                targets.insert(target);
            }
        }
    }

    targets
}

fn load_legacy_packages(
    toolchain_dir: &Path,
    log_target: &'static str,
) -> Result<Vec<Arc<Package>>, Report> {
    let mut packages = Vec::new();
    load_legacy_packages_from_dir(&toolchain_dir.join("lib"), log_target, &mut packages)?;
    load_legacy_packages_from_dir(toolchain_dir, log_target, &mut packages)?;
    Ok(packages)
}

fn load_legacy_packages_from_dir(
    dir: &Path,
    log_target: &'static str,
    packages: &mut Vec<Arc<Package>>,
) -> Result<(), Report> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("masl")) {
            continue;
        }
        log::debug!(target: log_target, "loading legacy library {}", path.display());
        let bytes = std::fs::read(&path).into_diagnostic()?;
        let lib = Library::read_from_bytes(&bytes).map_err(|err| {
            Report::msg(format!("failed to load library '{}': {err}", path.display()))
        })?;
        let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("legacy-library");
        packages.push(package_from_library(name, Arc::new(lib)));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use miden_assembly_syntax::{
        Library,
        ast::PathBuf as MasmPathBuf,
        library::{LibraryExport, ProcedureExport as LibraryProcedureExport},
    };
    use miden_core::{
        mast::{BasicBlockNodeBuilder, MastForest, MastForestContributor},
        operations::Operation,
    };
    use miden_mast_package::{Dependency, Package, TargetType};

    use super::*;

    fn absolute_path(name: &str) -> Arc<miden_assembly_syntax::Path> {
        let path = MasmPathBuf::new(name).expect("invalid path");
        let path = path.as_path().to_absolute().into_owned();
        Arc::from(path.into_boxed_path())
    }

    fn build_library(export: &str, op: Operation) -> Arc<Library> {
        let mut forest = MastForest::new();
        let node_id = BasicBlockNodeBuilder::new(vec![op], Vec::new())
            .add_to_forest(&mut forest)
            .expect("failed to build basic block");
        forest.make_root(node_id);

        let path = absolute_path(export);
        let export = LibraryProcedureExport::new(node_id, path.clone());
        let exports = BTreeMap::from([(path, LibraryExport::Procedure(export))]);

        Arc::new(Library::new(Arc::new(forest), exports).expect("failed to build library"))
    }

    fn build_package(name: &str, version: &str, op: Operation) -> Arc<Package> {
        Package::from_library(
            name.into(),
            version.parse().unwrap(),
            TargetType::Library,
            build_library("test::pkg::entry", op),
            [],
        )
        .into()
    }

    #[test]
    fn resolves_exact_digest_when_semver_is_duplicated() {
        let first = build_package("dep", "1.0.0", Operation::Add);
        let second = build_package("dep", "1.0.0", Operation::Mul);
        assert_ne!(first.digest(), second.digest());

        let root = Package::from_library(
            "root".into(),
            "1.0.0".parse().unwrap(),
            TargetType::Library,
            build_library("test::root::entry", Operation::Add),
            [Dependency {
                name: "dep".into(),
                kind: TargetType::Library,
                version: second.version.clone(),
                digest: second.digest(),
            }],
        );

        let mut registry = LocalPackageRegistry::default();
        registry.install_package(first);
        registry.install_package(second.clone());

        let packages = registry.resolve_dependency_packages(&root).unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].digest(), second.digest());
    }
}
