use crate::component::{Component, ValidationResult};
use crate::efi;
use crate::model::{ComponentStatus, ComponentUpdatable, ContentMetadata, SavedState, Status};
use crate::{component, ipc};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use openat_ext::OpenatDirExt;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::prelude::*;
use std::path::Path;

/// Stored in /boot to describe our state; think of it like
/// a tiny rpm/dpkg database.  It's stored in /boot
pub(crate) const STATEFILE_DIR: &str = "boot";
pub(crate) const STATEFILE_NAME: &str = "bootupd-state.json";
pub(crate) const WRITE_LOCK_PATH: &str = "run/bootupd-lock";

/// A message sent from client to server
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ClientRequest {
    /// Update a component
    Update { component: String },
    /// Validate a component
    Validate { component: String },
    /// Print the current state
    Status,
}

pub(crate) fn install(source_root: &str, dest_root: &str) -> Result<()> {
    let statepath = Path::new(dest_root)
        .join(STATEFILE_DIR)
        .join(STATEFILE_NAME);
    if statepath.exists() {
        bail!("{:?} already exists, cannot re-install", statepath);
    }

    let components = get_components();
    if components.is_empty() {
        println!("No components available for this platform.");
        return Ok(());
    }
    let mut state = SavedState {
        installed: Default::default(),
        pending: Default::default(),
    };
    for component in components {
        let meta = component.install(source_root, dest_root)?;
        state.installed.insert(component.name().into(), meta);
    }

    let sysroot = openat::Dir::open(dest_root)?;
    update_state(&sysroot, &state)?;

    Ok(())
}

pub(crate) fn get_components() -> Vec<Box<dyn Component>> {
    let mut components: Vec<Box<dyn Component>> = Vec::new();

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    components.push(Box::new(efi::EFI::default()));

    // #[cfg(target_arch = "x86_64")]
    // components.push(Box::new(bios::BIOS::new()));

    components
}

pub(crate) fn generate_update_metadata(sysroot_path: &str) -> Result<()> {
    for component in get_components() {
        let v = component.generate_update_metadata(sysroot_path)?;
        println!(
            "Generated update layout for {}: {}",
            component.name(),
            v.version,
        );
    }

    Ok(())
}

/// Hold a lock on the system root; while ordinarily we run
/// as a systemd unit which implicitly ensures a "singleton"
/// instance this is a double check.
fn acquire_write_lock<P: AsRef<Path>>(sysroot: P) -> Result<std::fs::File> {
    let sysroot = sysroot.as_ref();
    let lockf = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(sysroot.join(WRITE_LOCK_PATH))?;
    lockf.lock_exclusive()?;
    Ok(lockf)
}

/// Return value from daemon → client for component update
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ComponentUpdateResult {
    AtLatestVersion,
    Updated {
        previous: ContentMetadata,
        interrupted: Option<ContentMetadata>,
        new: ContentMetadata,
    },
}

/// daemon implementation of component update
pub(crate) fn update(name: &str) -> Result<ComponentUpdateResult> {
    let sysroot = openat::Dir::open("/")?;
    let _lock = acquire_write_lock("/")?;
    let mut state = get_saved_state("/")?.unwrap_or_else(|| SavedState {
        ..Default::default()
    });
    let component = component::new_from_name(name)?;
    let inst = if let Some(inst) = state.installed.get(name) {
        inst.clone()
    } else {
        anyhow::bail!("Component {} is not installed", name);
    };
    let update = component.query_update()?;
    let update = match update.as_ref() {
        Some(p) if inst.meta.can_upgrade_to(&p) => p,
        _ => return Ok(ComponentUpdateResult::AtLatestVersion),
    };
    let mut pending_container = state.pending.take().unwrap_or_default();
    let interrupted = pending_container.get(component.name()).cloned();

    pending_container.insert(component.name().into(), update.clone());
    update_state(&sysroot, &state)?;
    let newinst = component
        .run_update(&inst)
        .with_context(|| format!("Failed to update {}", component.name()))?;
    state.installed.insert(component.name().into(), newinst);
    pending_container.remove(component.name());
    update_state(&sysroot, &state)?;
    Ok(ComponentUpdateResult::Updated {
        previous: inst.meta,
        interrupted,
        new: update.clone(),
    })
}

/// daemon implementation of component validate
pub(crate) fn validate(name: &str) -> Result<ValidationResult> {
    let state = get_saved_state("/")?.unwrap_or_else(|| SavedState {
        ..Default::default()
    });
    let component = component::new_from_name(name)?;
    let inst = if let Some(inst) = state.installed.get(name) {
        inst.clone()
    } else {
        anyhow::bail!("Component {} is not installed", name);
    };
    component.validate(&inst)
}

/// Atomically replace the on-disk state with a new version
fn update_state(sysroot_dir: &openat::Dir, state: &SavedState) -> Result<()> {
    let subdir = sysroot_dir.sub_dir(STATEFILE_DIR)?;
    let f = {
        let f = subdir.new_unnamed_file(0o644)?;
        let mut buff = std::io::BufWriter::new(f);
        serde_json::to_writer(&mut buff, state)?;
        buff.flush()?;
        buff.into_inner()?
    };
    let dest_tmp_name = {
        // expect OK because we just created the filename above from a constant
        let mut buf = std::ffi::OsString::from(STATEFILE_NAME);
        buf.push(".tmp");
        buf
    };
    let dest_tmp_name = Path::new(&dest_tmp_name);
    if subdir.exists(dest_tmp_name)? {
        subdir.remove_file(dest_tmp_name)?;
    }
    subdir.link_file_at(&f, dest_tmp_name)?;
    f.sync_all()?;
    subdir.local_rename(dest_tmp_name, STATEFILE_NAME)?;
    Ok(())
}

/// Load the JSON file containing on-disk state
fn get_saved_state(sysroot_path: &str) -> Result<Option<SavedState>> {
    let sysroot_dir = openat::Dir::open(sysroot_path)
        .with_context(|| format!("opening sysroot {}", sysroot_path))?;

    let statefile_path = Path::new(STATEFILE_DIR).join(STATEFILE_NAME);
    let saved_state = if let Some(statusf) = sysroot_dir.open_file_optional(&statefile_path)? {
        let bufr = std::io::BufReader::new(statusf);
        let saved_state: SavedState = serde_json::from_reader(bufr)?;
        Some(saved_state)
    } else {
        None
    };
    Ok(saved_state)
}

pub(crate) fn status() -> Result<Status> {
    let mut ret: Status = Default::default();
    let state = if let Some(state) = get_saved_state("/")? {
        state
    } else {
        return Ok(ret);
    };
    for (name, ic) in state.installed.iter() {
        let component = crate::component::new_from_name(&name)?;
        let component = component.as_ref();
        let interrupted = state
            .pending
            .as_ref()
            .map(|p| p.get(name.as_str()))
            .flatten();
        let update = component.query_update()?;
        let updatable = ComponentUpdatable::from_metadata(&ic.meta, update.as_ref());
        ret.components.insert(
            name.to_string(),
            ComponentStatus {
                installed: ic.meta.clone(),
                interrupted: interrupted.cloned(),
                update,
                updatable,
            },
        );
    }
    Ok(ret)
}

pub(crate) fn print_status(status: &Status) {
    for (name, component) in status.components.iter() {
        println!("Component {}", name);
        println!("  Installed: {}", component.installed.version);

        if let Some(i) = component.interrupted.as_ref() {
            println!(
                "  WARNING: Previous update to {} was interrupted",
                i.version
            );
        }
        let msg = match component.updatable {
            ComponentUpdatable::NoUpdateAvailable => Cow::Borrowed("No update found"),
            ComponentUpdatable::AtLatestVersion => Cow::Borrowed("At latest version"),
            ComponentUpdatable::WouldDowngrade => Cow::Borrowed("Ignoring downgrade"),
            ComponentUpdatable::Upgradable => Cow::Owned(format!(
                "Available: {}",
                component.update.as_ref().expect("update").version
            )),
        };
        println!("  Update: {}", msg);
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let boot_method = if Path::new("/sys/firmware/efi").exists() {
            "EFI"
        } else {
            "BIOS"
        };
        println!("Boot method: {}", boot_method);
    }
}

/// Checks that the user has provided an environment variable to signal
/// acceptance of our alpha state - use this when performing write operations.
fn validate_preview_env() -> Result<()> {
    let v = "BOOTUPD_ACCEPT_PREVIEW";
    if std::env::var_os(v).is_none() {
        Err(anyhow::anyhow!(
            "bootupd is currently alpha; set {}=1 in environment to continue",
            v
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn client_run_update(c: &mut ipc::ClientToDaemonConnection) -> Result<()> {
    validate_preview_env()?;
    let status: Status = c.send(&ClientRequest::Status)?;
    if status.components.is_empty() {
        println!("No components installed.");
        return Ok(());
    }
    let mut updated = false;
    for (name, cstatus) in status.components.iter() {
        match cstatus.updatable {
            ComponentUpdatable::Upgradable => {}
            _ => continue,
        };
        match c.send(&ClientRequest::Update {
            component: name.to_string(),
        })? {
            ComponentUpdateResult::AtLatestVersion => {
                // Shouldn't happen unless we raced with another client
                eprintln!(
                    "warning: Expected update for {}, raced with a different client?",
                    name
                );
                continue;
            }
            ComponentUpdateResult::Updated {
                previous: _,
                interrupted,
                new,
            } => {
                if let Some(i) = interrupted {
                    eprintln!(
                        "warning: Continued from previous interrupted update: {}",
                        i.version,
                    );
                }
                println!("Updated {}: {}", name, new.version);
            }
        }
        updated = true;
    }
    if !updated {
        println!("No update available for any component.");
    }
    Ok(())
}

pub(crate) fn client_run_validate(c: &mut ipc::ClientToDaemonConnection) -> Result<()> {
    let status: Status = c.send(&ClientRequest::Status)?;
    if status.components.is_empty() {
        println!("No components installed.");
        return Ok(());
    }
    let mut caught_validation_error = false;
    for (name, _) in status.components.iter() {
        match c.send(&ClientRequest::Validate {
            component: name.to_string(),
        })? {
            ValidationResult::Valid => {
                println!("Validated: {}", name);
            }
            ValidationResult::Errors(errs) => {
                for err in errs {
                    eprintln!("{}", err);
                }
                caught_validation_error = true;
            }
        }
    }
    if caught_validation_error {
        anyhow::bail!("Caught validation errors");
    }
    Ok(())
}
