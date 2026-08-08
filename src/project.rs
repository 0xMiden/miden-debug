use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

use miden_assembly::DefaultSourceManager;
use miden_assembly_syntax::diagnostics::Report;

use crate::{DebuggerConfig, ExecutionConfig, InputFile};

/// Resolve the current Miden project's package and inputs when invoked through `miden`.
pub fn resolve_midenup_project(config: &mut DebuggerConfig) -> Result<(), Report> {
    resolve_project_input(config, std::env::var_os("MIDENUP_TOOLCHAIN").is_some(), run_miden_build)
}

fn resolve_project_input<F>(
    config: &mut DebuggerConfig,
    invoked_by_midenup: bool,
    mut build_project: F,
) -> Result<(), Report>
where
    F: FnMut(&Path) -> Result<(), Report>,
{
    if !invoked_by_midenup || has_explicit_execution_source(config) {
        return Ok(());
    }

    let working_dir = config.working_dir().into_owned();
    let Some(project_root) = find_project_root(&working_dir) else {
        return Ok(());
    };

    let manifest_path = project_root.join("miden-project.toml");
    let source_manager = DefaultSourceManager::default();
    let project = miden_project::Project::load(&manifest_path, &source_manager).map_err(|err| {
        Report::msg(format!(
            "failed to load Miden project from '{}': {err}",
            manifest_path.display()
        ))
    })?;
    let project_package = project.package();
    let package_name = project_package.name().into_inner();

    let package_path = match find_project_package(&project_root, package_name.as_ref()) {
        Some(path) => path,
        None => {
            eprintln!(
                "No compiled package found for project '{}'; running `miden build`...",
                package_name
            );
            build_project(&project_root)?;
            find_project_package(&project_root, package_name.as_ref()).ok_or_else(|| {
                Report::msg(format!(
                    "`miden build` completed, but no package for project '{}' was found under '{}'",
                    package_name,
                    project_root.join("target").display()
                ))
            })?
        }
    };

    eprintln!("Debugging project package '{}'.", package_path.display());
    config.input = Some(InputFile::from_path(package_path));

    let inputs_path = project_root.join("inputs.toml");
    if config.inputs.is_none() && inputs_path.is_file() {
        config.inputs = Some(ExecutionConfig::parse_file(&inputs_path).map_err(|err| {
            Report::msg(format!(
                "failed to load project inputs from '{}': {err}",
                inputs_path.display()
            ))
        })?);
    }

    Ok(())
}

fn has_explicit_execution_source(config: &DebuggerConfig) -> bool {
    if config.input.is_some() || config.replay.is_some() {
        return true;
    }

    #[cfg(feature = "dap")]
    if config.dap_connect.is_some() {
        return true;
    }

    false
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|directory| directory.join("miden-project.toml").is_file())
        .map(Path::to_path_buf)
}

fn find_project_package(project_root: &Path, package_name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for output_root in [
        project_root.join("target").join("miden"),
        project_root.join("target").join("midenc").join("miden"),
    ] {
        let Ok(profiles) = fs::read_dir(output_root) else {
            continue;
        };
        for profile in profiles.flatten() {
            let profile_path = profile.path();
            if !profile_path.is_dir() {
                continue;
            }
            let Ok(artifacts) = fs::read_dir(profile_path) else {
                continue;
            };
            for artifact in artifacts.flatten() {
                let path = artifact.path();
                if !is_project_package_path(&path, package_name) {
                    continue;
                }
                let modified = artifact
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH);
                candidates.push((modified, path));
            }
        }
    }

    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    candidates.pop().map(|(_, path)| path)
}

fn is_project_package_path(path: &Path, package_name: &str) -> bool {
    if !path.is_file()
        || path.extension().is_none_or(|extension| !extension.eq_ignore_ascii_case("masp"))
    {
        return false;
    }

    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    stem == package_name
        || stem.strip_prefix(package_name).is_some_and(|suffix| suffix.starts_with(':'))
}

fn run_miden_build(project_root: &Path) -> Result<(), Report> {
    let status = Command::new("miden")
        .arg("build")
        .current_dir(project_root)
        .status()
        .map_err(|err| Report::msg(format!("failed to run `miden build`: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Report::msg(format!(
            "`miden build` failed with status {}",
            status.code().unwrap_or(1)
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs};

    use super::*;

    fn write_project(root: &Path) {
        fs::write(
            root.join("miden-project.toml"),
            concat!(
                "[package]\n",
                "name = \"demo\"\n",
                "version = \"0.1.0\"\n",
                "\n",
                "[[bin]]\n",
                "name = \"demo\"\n",
                "path = \"<virtual>\"\n",
            ),
        )
        .unwrap();
    }

    #[test]
    fn resolves_project_package_and_inputs_for_midenup_invocation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_project(root);
        fs::write(root.join("inputs.toml"), "[inputs]\nstack = [25]\n").unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("target/miden/dev")).unwrap();
        fs::write(root.join("target/miden/dev/unrelated.masp"), b"unrelated").unwrap();
        let package_path = root.join("target/miden/dev/demo.masp");
        fs::write(&package_path, b"package").unwrap();

        let mut config = DebuggerConfig {
            working_dir: Some(root.join("src/nested")),
            ..Default::default()
        };
        resolve_project_input(&mut config, true, |_| panic!("project should not be rebuilt"))
            .unwrap();

        assert!(matches!(config.input, Some(InputFile::Real(ref path)) if path == &package_path));
        assert!(config.inputs.is_some());
    }

    #[test]
    fn builds_project_when_matching_package_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_project(root);
        let build_called = Cell::new(false);
        let mut config = DebuggerConfig {
            working_dir: Some(root.to_path_buf()),
            ..Default::default()
        };

        resolve_project_input(&mut config, true, |project_root| {
            build_called.set(true);
            let output = project_root.join("target/miden/dev");
            fs::create_dir_all(&output).unwrap();
            fs::write(output.join("demo.masp"), b"package").unwrap();
            Ok(())
        })
        .unwrap();

        assert!(build_called.get());
        assert!(
            matches!(config.input, Some(InputFile::Real(ref path)) if path.ends_with("target/miden/dev/demo.masp"))
        );
    }

    #[test]
    fn leaves_explicit_and_unmanaged_invocations_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let explicit_path = temp.path().join("explicit.masp");
        fs::write(&explicit_path, b"package").unwrap();
        let mut explicit = DebuggerConfig {
            input: Some(InputFile::from_path(&explicit_path)),
            working_dir: Some(temp.path().to_path_buf()),
            ..Default::default()
        };
        resolve_project_input(&mut explicit, true, |_| panic!("project should not be built"))
            .unwrap();
        assert!(
            matches!(explicit.input, Some(InputFile::Real(ref path)) if path == &explicit_path)
        );

        let mut unmanaged = DebuggerConfig {
            working_dir: Some(temp.path().to_path_buf()),
            ..Default::default()
        };
        resolve_project_input(&mut unmanaged, false, |_| panic!("project should not be built"))
            .unwrap();
        assert!(unmanaged.input.is_none());
    }

    #[test]
    fn leaves_replay_and_remote_dap_invocations_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        write_project(temp.path());

        let mut replay = DebuggerConfig {
            replay: Some(temp.path().join("session.mdsnap")),
            working_dir: Some(temp.path().to_path_buf()),
            ..Default::default()
        };
        resolve_project_input(&mut replay, true, |_| panic!("project should not be built"))
            .unwrap();
        assert!(replay.input.is_none());

        #[cfg(feature = "dap")]
        {
            let mut remote = DebuggerConfig {
                dap_connect: Some("127.0.0.1:4711".to_string()),
                working_dir: Some(temp.path().to_path_buf()),
                ..Default::default()
            };
            resolve_project_input(&mut remote, true, |_| panic!("project should not be built"))
                .unwrap();
            assert!(remote.input.is_none());
        }
    }

    #[test]
    fn recognizes_legacy_target_qualified_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("target/midenc/miden/dev");
        fs::create_dir_all(&output).unwrap();
        let package_path = output.join("demo:demo.masp");
        fs::write(&package_path, b"package").unwrap();

        assert_eq!(find_project_package(temp.path(), "demo"), Some(package_path));
    }
}
