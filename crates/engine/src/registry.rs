use alloc::{collections::BTreeMap, sync::Arc};
#[cfg(feature = "std")]
use std::path::{Path, PathBuf};

use miden_assembly_syntax::{
    Report,
    diagnostics::{Diagnostic, miette},
};
use miden_mast_package::Package;
use miden_package_registry::{
    PackageCache, PackageId, PackageIndex, PackageProvider, PackageRecord, PackageRegistry,
    PackageStore, PackageVersions, VersionRequirement,
};

type FxHashMap<K, V> = hashbrown::HashMap<K, V, rustc_hash::FxBuildHasher>;

#[cfg(feature = "std")]
use crate::LinkLibrary;

#[derive(Debug, thiserror::Error, Diagnostic)]
enum InstallPackageError {
    #[error("package {package}@{version} is already registered under a different digest")]
    AlreadyInstalledWithDifferentDigest {
        package: PackageId,
        version: miden_package_registry::Version,
    },
}

/// The in-memory package registry used during debugger execution.
///
/// This is initialized per-session, or on an as-needed basis.
///
/// It can be constructed in various ways, but the recommended way to use it is
/// [HybridPackageRegistry::new], which loads packages from the local filesystem registry (if
/// available), and adds in any libraries requested explicitly via `-l`.
pub struct HybridPackageRegistry {
    packages: FxHashMap<PackageId, PackageVersions>,
    artifacts: FxHashMap<PackageId, BTreeMap<miden_package_registry::Version, Arc<Package>>>,
}

impl HybridPackageRegistry {
    /// Get an empty, uninitialized registry
    pub fn empty() -> Self {
        Self {
            packages: Default::default(),
            artifacts: Default::default(),
        }
    }

    /// Get a new instance of the registry from compiled package inputs.
    #[cfg(feature = "std")]
    pub fn new(
        sysroot: Option<&Path>,
        search_paths: &[PathBuf],
        link_libraries: &[LinkLibrary],
    ) -> Result<Self, Report> {
        // Load system libraries
        let mut registry = if let Some(sysroot) = sysroot {
            Self::from_local_registry(sysroot)?
        } else {
            Self::empty()
        };

        // Load link libraries
        for lib in link_libraries {
            let package = lib.load(search_paths)?;
            match registry.install_if_missing(package) {
                Ok(_) => (),
                // Ignore duplicates when initializing the registry
                Err(InstallPackageError::AlreadyInstalledWithDifferentDigest { .. }) => (),
            }
        }

        Ok(registry)
    }

    /// Get a new instance of the registry seeded with packages available in the local filesystem-
    /// based package store.
    ///
    /// This returns an error if `--sysroot` was not provided/set.
    #[cfg(feature = "std")]
    pub fn from_local_registry(sysroot: &Path) -> Result<Self, Report> {
        let lib_dir = sysroot.join("lib");
        let entries = lib_dir.read_dir().map_err(|err| {
            Report::msg(format!("cannot read from sysroot ({}): {err}", lib_dir.display()))
        })?;

        let mut registry = Self::empty();
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            if path.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("masp")) {
                continue;
            }

            let package = crate::package::load_package_from_path(&path)?;
            match registry.install_if_missing(package) {
                Ok(_) => (),
                // Ignore duplicates when initializing the registry
                Err(InstallPackageError::AlreadyInstalledWithDifferentDigest { .. }) => (),
            }
        }

        Ok(registry)
    }

    pub fn all(&self) -> impl IntoIterator<Item = Arc<Package>> {
        self.artifacts.values().flat_map(|versions| versions.values().cloned())
    }

    fn install_if_missing(
        &mut self,
        package: Arc<Package>,
    ) -> Result<miden_package_registry::Version, InstallPackageError> {
        use alloc::collections::btree_map::Entry as BTreeMapEntry;

        use hashbrown::hash_map::Entry;

        let version = miden_package_registry::Version::new(
            package.version.clone(),
            package.dependency_commitment(),
        );
        log::trace!(target: "package-registry", "preparing to install package {}@{version}", package.name);
        let record = PackageRecord::new(
            version.clone(),
            package.manifest.dependencies().map(|dep| {
                (
                    dep.name.clone(),
                    VersionRequirement::Exact(miden_package_registry::Version::new(
                        dep.version.clone(),
                        dep.digest,
                    )),
                )
            }),
        );
        match self.packages.entry(package.name.clone()) {
            Entry::Occupied(mut entry) => {
                let versions = entry.get_mut();
                match versions.entry(package.version.clone()) {
                    BTreeMapEntry::Occupied(mut prev) => {
                        let prev_digest = prev.get().digest().copied();
                        if prev_digest.is_none_or(|prev_digest| {
                            prev_digest == package.dependency_commitment()
                        }) {
                            prev.insert(record);
                        } else {
                            log::trace!(target: "package-registry", "package already installed: {}@{version}", package.name);
                            return Err(InstallPackageError::AlreadyInstalledWithDifferentDigest {
                                package: package.name.clone(),
                                version,
                            });
                        }
                    }
                    BTreeMapEntry::Vacant(entry) => {
                        entry.insert(record);
                    }
                }
            }
            Entry::Vacant(entry) => {
                entry.insert([(package.version.clone(), record)].into_iter().collect());
            }
        }

        log::trace!(target: "package-registry", "installed {}@{version}", package.name);

        self.artifacts
            .entry(package.name.clone())
            .or_default()
            .insert(version.clone(), package);

        Ok(version)
    }
}

impl HybridPackageRegistry {
    fn insert_record(&mut self, id: PackageId, record: PackageRecord) {
        self.packages
            .entry(id)
            .or_default()
            .insert(record.semantic_version().clone(), record);
    }
}

impl PackageRegistry for HybridPackageRegistry {
    fn available_versions(&self, package: &PackageId) -> Option<&PackageVersions> {
        self.packages.get(package)
    }
}

impl PackageIndex for HybridPackageRegistry {
    type Error = Report;

    fn register(&mut self, name: PackageId, record: PackageRecord) -> Result<(), Self::Error> {
        if self.is_semver_available(&name, record.semantic_version()) {
            return Err(Report::msg(format!(
                "cannot register {name}: version {} is already registered",
                record.semantic_version()
            )));
        }
        self.insert_record(name, record);
        Ok(())
    }
}

impl PackageProvider for HybridPackageRegistry {
    fn load_package(
        &self,
        package: &PackageId,
        version: &miden_package_registry::Version,
    ) -> Result<Arc<Package>, Report> {
        let found = self.artifacts.get(package).and_then(|versions| versions.get(&version.version));
        match found {
            Some(artifact) if version.digest != Some(artifact.dependency_commitment()) => {
                Err(Report::msg(format!(
                    "cannot load {package}@{version}: a specific digest was requested, but \
                     differs from the available version"
                )))
            }
            Some(artifact) => Ok(Arc::clone(artifact)),
            None => Err(Report::msg(format!(
                "cannot load {package}@{version}: no such package available",
            ))),
        }
    }
}

impl PackageCache for HybridPackageRegistry {
    type Error = Report;

    fn cache_package(
        &mut self,
        package: Arc<Package>,
    ) -> Result<miden_package_registry::Version, Self::Error> {
        self.install_if_missing(package).map_err(Report::from)
    }
}

impl PackageStore for HybridPackageRegistry {
    fn publish_package(
        &mut self,
        package: Arc<Package>,
    ) -> Result<miden_package_registry::Version, Self::Error> {
        self.install_if_missing(package).map_err(Report::from)
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use std::string::ToString;

    use miden_assembly::{Assembler, DefaultSourceManager};
    use miden_core::serde::Serializable;

    use super::*;

    fn package(source: &str) -> Arc<Package> {
        Arc::new(
            *Assembler::new(Arc::new(DefaultSourceManager::default()))
                .assemble_program("registry-test", source)
                .unwrap(),
        )
    }

    #[test]
    fn cache_publish_and_load_preserve_versions_and_digests() {
        let original = package("begin push.1 end");
        let mut registry = HybridPackageRegistry::empty();
        let version = registry.cache_package(original.clone()).unwrap();
        assert_eq!(registry.available_versions(&original.name).unwrap().len(), 1);
        assert_eq!(registry.publish_package(original.clone()).unwrap(), version);
        assert!(Arc::ptr_eq(
            &registry.load_package(&original.name, &version).unwrap(),
            &original
        ));
        assert_eq!(registry.all().into_iter().count(), 1);

        let conflicting = package("begin push.2 end");
        assert_ne!(original.dependency_commitment(), conflicting.dependency_commitment());
        let error = registry.cache_package(conflicting.clone()).unwrap_err();
        assert!(error.to_string().contains("different digest"));
        let conflicting_version = miden_package_registry::Version::new(
            conflicting.version.clone(),
            conflicting.dependency_commitment(),
        );
        assert!(
            registry
                .load_package(&original.name, &conflicting_version)
                .unwrap_err()
                .to_string()
                .contains("specific digest")
        );
        assert!(Arc::ptr_eq(
            &registry.load_package(&original.name, &version).unwrap(),
            &original
        ));

        let mut newer = (*original).clone();
        newer.version = "1.0.0".parse().unwrap();
        let newer = Arc::new(newer);
        let newer_version = registry.publish_package(newer.clone()).unwrap();
        assert_eq!(registry.available_versions(&original.name).unwrap().len(), 2);
        assert!(Arc::ptr_eq(
            &registry.load_package(&original.name, &newer_version).unwrap(),
            &newer
        ));
        assert_eq!(registry.all().into_iter().count(), 2);

        let absent: PackageId = "missing".into();
        assert!(registry.available_versions(&absent).is_none());
        assert!(
            registry
                .load_package(&absent, &version)
                .unwrap_err()
                .to_string()
                .contains("no such package")
        );
    }

    #[test]
    fn registration_rejects_duplicates_without_replacing_records() {
        let original = package("begin push.1 end");
        let version = miden_package_registry::Version::new(
            original.version.clone(),
            original.dependency_commitment(),
        );
        let record = PackageRecord::new(version.clone(), []);
        let mut registry = HybridPackageRegistry::empty();
        registry.register(original.name.clone(), record.clone()).unwrap();
        let error = registry.register(original.name.clone(), record).unwrap_err();
        assert!(error.to_string().contains("already registered"));
        assert_eq!(registry.available_versions(&original.name).unwrap().len(), 1);
        assert_eq!(registry.cache_package(original.clone()).unwrap(), version);
        assert_eq!(registry.all().into_iter().count(), 1);
    }

    #[test]
    fn local_registry_loads_only_packages_and_reports_invalid_inputs() {
        let directory = tempfile::tempdir().unwrap();
        assert!(HybridPackageRegistry::from_local_registry(directory.path()).is_err());
        let lib = directory.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(lib.join("ignored.txt"), b"not a package").unwrap();
        std::fs::write(lib.join("no-extension"), b"not a package").unwrap();
        let original = package("begin push.1 end");
        std::fs::write(lib.join("library.MASP"), original.to_bytes()).unwrap();
        let registry = HybridPackageRegistry::from_local_registry(directory.path()).unwrap();
        assert_eq!(registry.all().into_iter().count(), 1);
        assert!(registry.available_versions(&original.name).is_some());
        std::fs::write(lib.join("broken.masp"), b"not a package").unwrap();
        assert!(HybridPackageRegistry::from_local_registry(directory.path()).is_err());
    }

    #[test]
    fn explicit_libraries_do_not_replace_conflicting_sysroot_packages() {
        use crate::Linkage;

        let directory = tempfile::tempdir().unwrap();
        let lib = directory.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        let original = package("begin push.1 end");
        let conflicting = package("begin push.2 end");
        std::fs::write(lib.join("original.masp"), original.to_bytes()).unwrap();
        let path = directory.path().join("conflicting.masp");
        std::fs::write(&path, conflicting.to_bytes()).unwrap();
        let library = LinkLibrary {
            name: original.name.to_string().into(),
            path: Some(path),
            linkage: Linkage::Dynamic,
        };
        let registry = HybridPackageRegistry::new(
            Some(directory.path()),
            &[],
            core::slice::from_ref(&library),
        )
        .unwrap();
        let loaded = registry.all().into_iter().next().unwrap();
        assert_eq!(loaded.dependency_commitment(), original.dependency_commitment());
        let registry = HybridPackageRegistry::new(None, &[], &[library]).unwrap();
        let loaded = registry.all().into_iter().next().unwrap();
        assert_eq!(loaded.dependency_commitment(), conflicting.dependency_commitment());
    }
}
