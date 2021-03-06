// Copyright 2017 CoreOS, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Providers
//!
//! These are the providers which Afterburn knows how to retrieve metadata
//! from. Internally, they handle the ins and outs of each providers metadata
//! services, and externally, they provide a function to fetch that metadata in
//! a regular format.
//!
//! To add a provider, put a `pub mod provider;` line in this file, export a
//! function to fetch the metadata, and then add a match line in the top-level
//! `fetch_metadata()` function in metadata.rs.

pub mod aws;
pub mod azure;
pub mod cloudstack;
pub mod digitalocean;
pub mod gcp;
pub mod openstack;
pub mod packet;
pub mod vagrant_virtualbox;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::prelude::*;
use std::path::Path;

use openssh_keys::PublicKey;
use users::{self, User};

use crate::errors::*;
use crate::network;

#[cfg(not(feature = "cl-legacy"))]
const ENV_PREFIX: &str = "AFTERBURN_";
#[cfg(feature = "cl-legacy")]
const ENV_PREFIX: &str = "COREOS_";

fn create_file(filename: &str) -> Result<File> {
    let file_path = Path::new(&filename);
    // create the directories if they don't exist
    let folder = file_path
        .parent()
        .ok_or_else(|| format!("could not get parent directory of {:?}", file_path))?;
    fs::create_dir_all(&folder).chain_err(|| format!("failed to create directory {:?}", folder))?;
    // create (or truncate) the file we want to write to
    File::create(file_path).chain_err(|| format!("failed to create file {:?}", file_path))
}

#[cfg(feature = "cl-legacy")]
fn write_ssh_keys(user: User, ssh_keys: Vec<PublicKey>) -> Result<()> {
    use update_ssh_keys::{AuthorizedKeyEntry, AuthorizedKeys};

    // If we don't have any SSH keys, don't bother trying to write them as
    // update-ssh-keys will yell at us.
    if !ssh_keys.is_empty() {
        // open the user's authorized keys directory
        let user_name = user.name().to_string_lossy().into_owned();
        let mut authorized_keys_dir = AuthorizedKeys::open(user, true, None).chain_err(|| {
            format!(
                "failed to open authorized keys directory for user '{}'",
                user_name
            )
        })?;

        // add the ssh keys to the directory
        let entries = ssh_keys
            .into_iter()
            .map(|key| AuthorizedKeyEntry::Valid { key })
            .collect::<Vec<_>>();
        // legacy name for legacy mode
        authorized_keys_dir.add_keys("coreos-metadata", entries, true, true)?;

        // write the changes and sync the directory
        authorized_keys_dir
            .write()
            .chain_err(|| "failed to update authorized keys directory")?;
        authorized_keys_dir
            .sync()
            .chain_err(|| "failed to update authorized keys")?;
    }

    Ok(())
}

#[cfg(not(feature = "cl-legacy"))]
fn write_ssh_keys(user: User, ssh_keys: Vec<PublicKey>) -> Result<()> {
    use std::io::ErrorKind::NotFound;
    use tempfile;
    use users::os::unix::UserExt;

    // switch users
    let _guard = users::switch::switch_user_group(user.uid(), user.primary_group_id())
        .chain_err(|| "failed to switch user/group")?;

    // get paths
    let dir_path = user.home_dir().join(".ssh").join("authorized_keys.d");
    let file_name = "afterburn";
    let file_path = &dir_path.join(file_name);

    if !ssh_keys.is_empty() {
        // ensure directory exists
        fs::create_dir_all(&dir_path)
            .chain_err(|| format!("failed to create directory {:?}", &dir_path))?;

        // create temporary file
        let mut temp_file = tempfile::Builder::new()
            .prefix(&format!(".{}-", file_name))
            .tempfile_in(&dir_path)
            .chain_err(|| "failed to create temporary file")?;

        // write out keys
        for key in ssh_keys {
            writeln!(temp_file, "{}", key).chain_err(|| {
                format!("failed to write to file {:?}", temp_file.path().display())
            })?;
        }

        // sync to disk
        temp_file
            .as_file()
            .sync_all()
            .chain_err(|| format!("failed to sync file {:?}", temp_file.path().display()))?;

        // atomically rename to destination
        // don't leak temporary file on error
        temp_file
            .persist(&file_path)
            .map_err(|e| {
                e.file.close().ok();
                e.error
            })
            .chain_err(|| format!("failed to persist file {:?}", file_path.display()))?;
    } else {
        // delete the file
        match fs::remove_file(&file_path) {
            Err(ref e) if e.kind() == NotFound => Ok(()),
            other => other,
        }
        .chain_err(|| format!("failed to remove file {:?}", file_path.display()))?;
    }

    // sync parent dir to persist updates
    let dir_file = File::open(&dir_path)
        .chain_err(|| format!("failed to open {:?} for syncing", dir_path.display()))?;
    dir_file
        .sync_all()
        .chain_err(|| format!("failed to sync {:?}", dir_path.display()))?;

    // make clippy happy while fulfilling our interface
    drop(user);

    Ok(())
}

pub trait MetadataProvider {
    fn attributes(&self) -> Result<HashMap<String, String>>;
    fn hostname(&self) -> Result<Option<String>>;
    fn ssh_keys(&self) -> Result<Vec<PublicKey>>;
    fn networks(&self) -> Result<Vec<network::Interface>>;
    fn network_devices(&self) -> Result<Vec<network::Device>>;
    fn boot_checkin(&self) -> Result<()>;

    fn write_attributes(&self, attributes_file_path: String) -> Result<()> {
        let mut attributes_file = create_file(&attributes_file_path)?;
        for (k, v) in self.attributes()? {
            writeln!(&mut attributes_file, "{}{}={}", ENV_PREFIX, k, v).chain_err(|| {
                format!("failed to write attributes to file {:?}", attributes_file)
            })?;
        }
        Ok(())
    }

    fn write_ssh_keys(&self, ssh_keys_user: String) -> Result<()> {
        let ssh_keys = self.ssh_keys()?;
        let user = users::get_user_by_name(&ssh_keys_user)
            .ok_or_else(|| format!("could not find user with username {:?}", ssh_keys_user))?;

        write_ssh_keys(user, ssh_keys)?;

        Ok(())
    }

    fn write_hostname(&self, hostname_file_path: String) -> Result<()> {
        match self.hostname()? {
            Some(ref hostname) => {
                let mut hostname_file = create_file(&hostname_file_path)?;
                writeln!(&mut hostname_file, "{}", hostname).chain_err(|| {
                    format!(
                        "failed to write hostname {:?} to file {:?}",
                        hostname, hostname_file
                    )
                })
            }
            None => Ok(()),
        }
    }

    fn write_network_units(&self, network_units_dir: String) -> Result<()> {
        let dir_path = Path::new(&network_units_dir);
        fs::create_dir_all(&dir_path)
            .chain_err(|| format!("failed to create directory {:?}", dir_path))?;
        for interface in &self.networks()? {
            let file_path = dir_path.join(interface.unit_name());
            let mut unit_file = File::create(&file_path)
                .chain_err(|| format!("failed to create file {:?}", file_path))?;
            write!(&mut unit_file, "{}", interface.config()).chain_err(|| {
                format!(
                    "failed to write network interface unit file {:?}",
                    unit_file
                )
            })?;
        }
        for device in &self.network_devices()? {
            let file_path = dir_path.join(device.unit_name());
            let mut unit_file = File::create(&file_path)
                .chain_err(|| format!("failed to create file {:?}", file_path))?;
            write!(&mut unit_file, "{}", device.config()).chain_err(|| {
                format!("failed to write network device unit file {:?}", unit_file)
            })?;
        }
        Ok(())
    }
}
