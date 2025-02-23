use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use log::info;
use oci_spec::runtime::SpecBuilder;
use serde::Deserialize;
use tempfile::{TempDir, tempdir_in};
use tokio::fs::{canonicalize, symlink};
use tokio::process::Command;
use tokio::time::sleep;
use tokio_async_drop::tokio_async_drop;

use super::Task;
use crate::containerd;
use crate::mocks::task_client::TaskClient;
use crate::protos::containerd::task::v2::ShutdownRequest;

pub struct Shim {
    dir: TempDir,
    client: TaskClient,
    containerd: containerd::Client,
}

impl Shim {
    pub(super) async fn new(
        containerd: containerd::Client,
        scratch: impl AsRef<Path>,
        verbose: bool,
        binary: impl AsRef<Path>,
    ) -> Result<Self> {
        info!("Setting up shim");

        let scratch = scratch.as_ref();

        let socket = scratch.join("containerd.sock.ttrpc");
        let dir = tempdir_in(scratch)?;

        let spec = SpecBuilder::default().build()?;
        spec.save(dir.path().join("config.json"))?;

        if verbose {
            let stderr = canonicalize("/proc/self/fd/2").await?;
            symlink(stderr, dir.path().join("log")).await?;
        } else {
            symlink("/dev/null", dir.path().join("log")).await?;
        }

        info!("Starting shim");

        let pid = std::process::id();

        let mut output = Output {
            status: ExitStatus::from_raw(1),
            stderr: b"timeout starting shim".to_vec(),
            stdout: b"".to_vec(),
        };

        let binary = binary.as_ref();
        let start_shim = || {
            Command::new(binary)
                .args([
                    "-namespace",
                    &format!("shim-benchmark-{pid}"),
                    "-id",
                    &format!("shim-benchmark-{pid}"),
                    "-address",
                    "/run/containerd/containerd.sock",
                    "start",
                ])
                .env("TTRPC_ADDRESS", &socket)
                .current_dir(dir.path())
                .output()
        };

        for _ in 0..10 {
            tokio::select! {
                out = start_shim() => {
                    output = out?;
                    if output.status.success() {
                        break;
                    }
                },
                _ = sleep(Duration::from_secs(2)) => {
                    // try again
                }
            }
        }

        if !output.status.success() {
            bail!("failed to start shim: {output:?}");
        }

        let mut address = String::from_utf8(output.stdout)?.trim().to_owned();
        if address.starts_with("{") {
            #[derive(Deserialize)]
            struct Address {
                address: String,
                protocol: String,
            }

            let parsed: Address = serde_json::from_str(&address)?;
            ensure!(parsed.protocol == "ttrpc");
            address = parsed.address;
        }

        println!("Connecting to {address}");
        let client = TaskClient::connect(address).await?;

        Ok(Shim {
            dir,
            client,
            containerd,
        })
    }
}

impl crate::traits::Shim for Shim {
    type Task = Task;

    async fn task<T: Into<String>>(
        &self,
        image: impl Into<String>,
        args: impl IntoIterator<Item = T>,
    ) -> Result<Task> {
        Task::new(
            self.containerd.clone(),
            &self.dir,
            image.into(),
            args,
            self.client.clone(),
        )
        .await
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        tokio_async_drop!({
            let _ = self
                .client
                .shutdown(ShutdownRequest {
                    now: true,
                    ..Default::default()
                })
                .await;
        })
    }
}
