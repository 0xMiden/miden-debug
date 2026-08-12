use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use miden_assembly::DefaultSourceManager;
use miden_assembly_syntax::diagnostics::Report;

use crate::{DebuggerConfig, ExecutionConfig, InputFile};

struct ProjectBuild<'a> {
    manifest_path: &'a Path,
    target_dir: &'a Path,
    target_name: &'a str,
    entrypoint: &'a str,
}

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
    F: FnMut(&ProjectBuild<'_>) -> Result<(), Report>,
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
    let [target] = project_package.executable_targets() else {
        return Err(Report::msg(format!(
            "project '{package_name}' must define exactly one executable target to use `miden \
             debug`"
        )));
    };
    let target_name = target.name.inner().as_ref();
    let target_dir = project_root.join("target").join("miden-debug");
    let package_path = target_dir.join("debug").join(format!("{package_name}:{target_name}.masp"));
    let entrypoint = format!("{}::entrypoint", target_name.replace('-', "_"));
    let build = ProjectBuild {
        manifest_path: &manifest_path,
        target_dir: &target_dir,
        target_name,
        entrypoint: &entrypoint,
    };

    eprintln!("Building debug package for project '{package_name}'...");
    build_project(&build)?;
    if !package_path.is_file() {
        return Err(Report::msg(format!(
            "`miden build` completed, but did not produce the requested package '{}'",
            package_path.display()
        )));
    }

    eprintln!("Debugging project package '{}'.", package_path.display());
    config.input = Some(InputFile::from_path(package_path));

    let inputs_path = working_dir.join("inputs.toml");
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

fn miden_build_command(build: &ProjectBuild<'_>) -> Command {
    let mut command = Command::new("miden");
    command
        .arg("build")
        .arg("--target-dir")
        .arg(build.target_dir)
        .arg("--profile")
        .arg("debug")
        .arg("--manifest-path")
        .arg(build.manifest_path)
        .arg("--target")
        .arg(build.target_name)
        .arg("--entrypoint")
        .arg(build.entrypoint);
    if let Some(project_root) = build.manifest_path.parent() {
        command.current_dir(project_root);
    }
    command
}

fn run_miden_build(build: &ProjectBuild<'_>) -> Result<(), Report> {
    fs::create_dir_all(build.target_dir).map_err(|err| {
        Report::msg(format!(
            "failed to create debugger target directory '{}': {err}",
            build.target_dir.display()
        ))
    })?;

    let status = miden_build_command(build)
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
    fn builds_debug_package_and_loads_inputs_from_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_project(root);
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/nested/inputs.toml"), "[inputs]\nstack = [25]\n").unwrap();
        let package_path = root.join("target/miden-debug/debug/demo:demo.masp");
        let manifest_path = root.join("miden-project.toml");
        let build_called = Cell::new(false);

        let mut config = DebuggerConfig {
            working_dir: Some(root.join("src/nested")),
            ..Default::default()
        };
        resolve_project_input(&mut config, true, |build| {
            build_called.set(true);
            assert_eq!(build.manifest_path, manifest_path);
            assert_eq!(build.target_dir, root.join("target/miden-debug"));
            assert_eq!(build.target_name, "demo");
            assert_eq!(build.entrypoint, "demo::entrypoint");
            fs::create_dir_all(package_path.parent().unwrap()).unwrap();
            fs::write(&package_path, b"package").unwrap();
            Ok(())
        })
        .unwrap();

        assert!(build_called.get());
        assert!(matches!(config.input, Some(InputFile::Real(ref path)) if path == &package_path));
        assert!(config.inputs.is_some());
    }

    #[test]
    fn rebuilds_project_even_when_output_exists() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_project(root);
        let output = root.join("target/miden-debug/debug/demo:demo.masp");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, b"stale").unwrap();
        let build_called = Cell::new(false);
        let mut config = DebuggerConfig {
            working_dir: Some(root.to_path_buf()),
            ..Default::default()
        };

        resolve_project_input(&mut config, true, |_| {
            build_called.set(true);
            fs::write(&output, b"fresh").unwrap();
            Ok(())
        })
        .unwrap();

        assert!(build_called.get());
        assert_eq!(fs::read(output).unwrap(), b"fresh");
    }

    #[test]
    fn does_not_load_inputs_from_project_root_when_working_in_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_project(root);
        fs::write(root.join("inputs.toml"), "[inputs]\nstack = [25]\n").unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        let mut config = DebuggerConfig {
            working_dir: Some(root.join("src/nested")),
            ..Default::default()
        };

        resolve_project_input(&mut config, true, |_| {
            let output = root.join("target/miden-debug/debug/demo:demo.masp");
            fs::create_dir_all(output.parent().unwrap()).unwrap();
            fs::write(output, b"package").unwrap();
            Ok(())
        })
        .unwrap();

        assert!(config.inputs.is_none());
    }

    #[test]
    fn preserves_explicit_inputs_in_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_project(root);
        fs::write(root.join("inputs.toml"), "not valid TOML").unwrap();
        let explicit_inputs = ExecutionConfig::parse_str("[inputs]\nstack = [99]\n").unwrap();
        let mut config = DebuggerConfig {
            inputs: Some(explicit_inputs),
            working_dir: Some(root.to_path_buf()),
            ..Default::default()
        };

        resolve_project_input(&mut config, true, |_| {
            let output = root.join("target/miden-debug/debug/demo:demo.masp");
            fs::create_dir_all(output.parent().unwrap()).unwrap();
            fs::write(output, b"package").unwrap();
            Ok(())
        })
        .unwrap();

        let inputs = config.inputs.unwrap().inputs.iter().copied().collect::<Vec<_>>();
        assert_eq!(inputs[0], miden_core::Felt::new(99).unwrap());
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
    fn build_command_uses_debug_profile_manifest_target_and_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("miden-project.toml");
        let target_dir = temp.path().join("target/miden-debug");
        let build = ProjectBuild {
            manifest_path: &manifest,
            target_dir: &target_dir,
            target_name: "demo-bin",
            entrypoint: "demo_bin::entrypoint",
        };
        let command = miden_build_command(&build);
        let args = command.get_args().collect::<Vec<_>>();

        assert_eq!(command.get_program(), "miden");
        assert_eq!(command.get_current_dir(), Some(temp.path()));
        assert_eq!(
            args,
            [
                "build".as_ref(),
                "--target-dir".as_ref(),
                target_dir.as_os_str(),
                "--profile".as_ref(),
                "debug".as_ref(),
                "--manifest-path".as_ref(),
                manifest.as_os_str(),
                "--target".as_ref(),
                "demo-bin".as_ref(),
                "--entrypoint".as_ref(),
                "demo_bin::entrypoint".as_ref(),
            ]
        );
    }
}
