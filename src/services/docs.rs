/*******************************************************************************
 * Copyright (c) 2024 Cénotélie Opérations SAS (cenotelie.fr)
 ******************************************************************************/

//! Docs generation and management

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use flate2::bufread::GzDecoder;
use futures::StreamExt;
use log::{error, info};
use sqlx::{Pool, Sqlite};
use tar::Archive;
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::model::config::Configuration;
use crate::model::JobCrate;
use crate::services::database::Database;
use crate::services::storage::Storage;
use crate::utils::apierror::{error_backend_failure, specialize, ApiError};
use crate::utils::concurrent::n_at_a_time;
use crate::utils::db::in_transaction;

/// Service to generate documentation for a crate
pub trait DocsGenerator {
    /// Queues a job for documentation generation
    fn queue(&self, job: JobCrate) -> Result<(), ApiError>;
}

/// Gets the documentation generation service
pub fn get_docs_generator(
    configuration: Arc<Configuration>,
    service_db_pool: Pool<Sqlite>,
    service_storage: Arc<Storage>,
) -> Arc<dyn DocsGenerator + Send + Sync> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let service = Arc::new(DocsGeneratorImpl {
        configuration,
        service_db_pool,
        service_storage,
        queue: sender,
    });
    let service2 = service.clone();
    let _handle = tokio::spawn(async move {
        service2.worker(receiver).await;
    });
    service
}

/// Service to generate documentation for a crate
#[derive(Clone)]
struct DocsGeneratorImpl {
    /// The configuration
    configuration: Arc<Configuration>,
    /// The database pool
    service_db_pool: Pool<Sqlite>,
    /// The storage layer
    service_storage: Arc<Storage>,
    /// The queue of waiting jobs
    queue: UnboundedSender<JobCrate>,
}

impl DocsGenerator for DocsGeneratorImpl {
    /// Queues a job for documentation generation
    fn queue(&self, job: JobCrate) -> Result<(), ApiError> {
        self.queue.send(job)?;
        Ok(())
    }
}

impl DocsGeneratorImpl {
    /// Implementation of the worker
    async fn worker(&self, receiver: UnboundedReceiver<JobCrate>) {
        let mut stream = UnboundedReceiverStream::new(receiver);
        while let Some(job) = stream.next().await {
            if let Err(e) = self.docs_worker_job(job).await {
                error!("{e}");
                if let Some(backtrace) = &e.backtrace {
                    error!("{backtrace}");
                }
            }
        }
    }

    /// Executes a documentation generation job
    async fn docs_worker_job(&self, job: JobCrate) -> Result<(), ApiError> {
        info!("generating doc for {} {}", job.name, job.version);
        let content = self.service_storage.download_crate(&job.name, &job.version).await?;
        let temp_folder = Self::extract_content(&job.name, &job.version, &content)?;
        let gen_is_ok = match self.generate_doc(&temp_folder).await {
            Ok(mut project_folder) => {
                project_folder.push("target");
                project_folder.push("doc");
                let doc_folder = project_folder;
                self.upload_package(&job.name, &job.version, &doc_folder).await?;
                true
            }
            Err(e) => {
                // upload the log
                let log = e.details.unwrap();
                let path = format!("{}/{}/log.txt", job.name, job.version);
                self.service_storage.store_doc_data(&path, log.into_bytes()).await?;
                false
            }
        };
        let mut connection = self.service_db_pool.acquire().await?;
        in_transaction(&mut connection, |transaction| async move {
            let database = Database::new(transaction);
            database.set_crate_documentation(&job.name, &job.version, gen_is_ok).await
        })
        .await?;
        tokio::fs::remove_dir_all(&temp_folder).await?;
        Ok(())
    }

    /// Generates and upload the documentation for a crate
    fn extract_content(name: &str, version: &str, content: &[u8]) -> Result<PathBuf, ApiError> {
        let decoder = GzDecoder::new(content);
        let mut archive = Archive::new(decoder);
        let target = format!("/tmp/{name}_{version}");
        archive.unpack(&target)?;
        Ok(PathBuf::from(target))
    }

    /// Generate the documentation for the package in a specific folder
    async fn generate_doc(&self, temp_folder: &Path) -> Result<PathBuf, ApiError> {
        let mut path: PathBuf = temp_folder.to_path_buf();
        // get the first sub dir
        let mut dir = tokio::fs::read_dir(&path).await?;
        let first = dir.next_entry().await?.unwrap();
        path = first.path();

        let mut command = Command::new("cargo");
        command
            .current_dir(&path)
            .arg("rustdoc")
            .arg("-Zunstable-options")
            .arg("-Zrustdoc-map")
            .arg("--all-features")
            .arg("--config")
            .arg("build.rustdocflags=[\"-Zunstable-options\",\"--extern-html-root-takes-precedence\"]")
            .arg("--config")
            .arg(format!(
                "doc.extern-map.registries.{}=\"{}/docs\"",
                self.configuration.self_local_name, self.configuration.web_public_uri
            ));
        for external in &self.configuration.external_registries {
            command.arg("--config").arg(format!(
                "doc.extern-map.registries.{}=\"{}\"",
                external.name, external.docs_root
            ));
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        drop(child.stdin.take()); // close stdin
        let output = child.wait_with_output().await?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error = format!("-- stdout\n{stdout}\n\n-- stderr\n{stderr}");
            return Err(specialize(error_backend_failure(), error));
        }
        Ok(path)
    }

    /// Uploads the documentation for package
    async fn upload_package(&self, name: &str, version: &str, doc_folder: &Path) -> Result<(), ApiError> {
        let files = Self::upload_package_find_files(doc_folder, &format!("{name}/{version}")).await?;
        let results = n_at_a_time(
            files.into_iter().map(|(key, path)| {
                let service_storage = self.service_storage.clone();
                Box::pin(async move { service_storage.store_doc_file(&key, &path).await })
            }),
            8,
            Result::is_err,
        )
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    /// Find target to upload in a folder and its sub-folders
    async fn upload_package_find_files(folder: &Path, prefix: &str) -> Result<Vec<(String, PathBuf)>, std::io::Error> {
        let mut results = Vec::new();
        let mut to_explore = vec![(folder.to_path_buf(), prefix.to_string())];
        while let Some((folder, prefix)) = to_explore.pop() {
            let mut dir = tokio::fs::read_dir(folder).await?;
            while let Some(entry) = dir.next_entry().await? {
                let entry_path = entry.path();
                let entry_type = entry.file_type().await?;
                if entry_type.is_file() {
                    results.push((format!("{prefix}/{}", entry.file_name().to_str().unwrap()), entry_path));
                } else if entry_type.is_dir() {
                    to_explore.push((entry_path, format!("{prefix}/{}", entry.file_name().to_str().unwrap())));
                }
            }
        }
        Ok(results)
    }
}
