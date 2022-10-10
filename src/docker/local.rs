use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::Ordering;

use super::shared::*;
use crate::errors::Result;
use crate::extensions::CommandExt;
use crate::file::{PathExt, ToUtf8};
use crate::shell::{MessageInfo, Stream};
use eyre::Context;

// NOTE: host path must be absolute
fn mount(docker: &mut Command, host_path: &Path, absolute_path: &Path, prefix: &str) -> Result<()> {
    let mount_path = absolute_path.as_posix_absolute()?;
    docker.args([
        "-v",
        &format!("{}:{prefix}{}:z", host_path.to_utf8()?, mount_path),
    ]);
    Ok(())
}

pub(crate) fn run(
    options: DockerOptions,
    paths: DockerPaths,
    args: &[String],
    msg_info: &mut MessageInfo,
) -> Result<ExitStatus> {
    let engine = &options.engine;
    let toolchain_dirs = paths.directories.toolchain_directories();
    let package_dirs = paths.directories.package_directories();

    let mut cmd = cargo_safe_command(options.cargo_variant);
    cmd.args(args);

    let mut docker = subcommand(engine, "run");
    docker_userns(&mut docker);

    options
        .image
        .platform
        .specify_platform(&options.engine, &mut docker);
    docker_envvars(&mut docker, &options, toolchain_dirs, msg_info)?;

    docker_mount(
        &mut docker,
        &options,
        &paths,
        |docker, host, absolute| mount(docker, host, absolute, ""),
        |_| {},
        msg_info,
    )?;

    let container = toolchain_dirs.unique_container_identifier(options.target.target())?;
    docker.args(["--name", &container]);
    docker.arg("--rm");

    docker_seccomp(&mut docker, engine.kind, &options.target, &paths.metadata)
        .wrap_err("when copying seccomp profile")?;
    docker_user_id(&mut docker, engine.kind);

    docker
        .args([
            "-v",
            &format!(
                "{}:{}:z",
                toolchain_dirs.xargo_host_path()?,
                toolchain_dirs.xargo_mount_path()
            ),
        ])
        .args([
            "-v",
            &format!(
                "{}:{}:z",
                toolchain_dirs.cargo_host_path()?,
                toolchain_dirs.cargo_mount_path()
            ),
        ])
        // Prevent `bin` from being mounted inside the Docker container.
        .args(["-v", &format!("{}/bin", toolchain_dirs.cargo_mount_path())]);
    docker.args([
        "-v",
        &format!(
            "{}:{}:z",
            package_dirs.host_root().to_utf8()?,
            package_dirs.mount_root()
        ),
    ]);
    docker
        .args([
            "-v",
            &format!(
                "{}:{}:z,ro",
                toolchain_dirs.get_sysroot().to_utf8()?,
                toolchain_dirs.sysroot_mount_path()
            ),
        ])
        .args([
            "-v",
            &format!("{}:/target:z", package_dirs.target().to_utf8()?),
        ]);
    docker_cwd(&mut docker, &paths)?;

    // When running inside NixOS or using Nix packaging we need to add the Nix
    // Store to the running container so it can load the needed binaries.
    if let Some(nix_store) = toolchain_dirs.nix_store() {
        docker.args([
            "-v",
            &format!(
                "{}:{}:z",
                nix_store.to_utf8()?,
                nix_store.as_posix_absolute()?
            ),
        ]);
    }

    if io::Stdin::is_atty() && io::Stdout::is_atty() && io::Stderr::is_atty() {
        docker.arg("-t");
    }
    let mut image_name = options.image.name.clone();
    if options.needs_custom_image() {
        image_name = options
            .custom_image_build(&paths, msg_info)
            .wrap_err("when building custom image")?;
    }

    Container::create(engine.clone(), container)?;
    let status = docker
        .arg(&image_name)
        .args(["sh", "-c", &build_command(toolchain_dirs, &cmd)])
        .run_and_get_status(msg_info, false)
        .map_err(Into::into);

    // `cargo` generally returns 0 or 101 on completion, but isn't guaranteed
    // to. `ExitStatus::code()` may be None if a signal caused the process to
    // terminate or it may be a known interrupt return status (130, 137, 143).
    // simpler: just test if the program termination handler was called.
    // SAFETY: an atomic load.
    let is_terminated = unsafe { crate::errors::TERMINATED.load(Ordering::SeqCst) };
    if !is_terminated {
        Container::exit_static();
    }

    status
}
