//! Open remote datasets through OpenDAL's object_store adapter.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use lance::{dataset::builder::DatasetBuilder, session::Session};
use lance_io::object_store::{
    DEFAULT_DOWNLOAD_RETRY_COUNT, ObjectStore, ObjectStoreParams, ObjectStoreProvider,
};
use object_store_opendal::OpendalStore;
use opendal::{Operator, services};
use url::Url;

/// OpenDAL owns remote I/O; Lance still resolves manifests and decodes data.
#[derive(Debug)]
struct S3Provider(Arc<OpendalStore>);

#[async_trait::async_trait]
impl ObjectStoreProvider for S3Provider {
    async fn new_store(
        &self,
        location: Url,
        params: &ObjectStoreParams,
    ) -> lance_core::Result<ObjectStore> {
        Ok(ObjectStore::new(
            self.0.clone(),
            location,
            params.block_size,
            None,
            false,
            false,
            1,
            DEFAULT_DOWNLOAD_RETRY_COUNT,
            None,
        ))
    }
}

pub(super) fn dataset_builder(source: &str) -> Result<(String, DatasetBuilder)> {
    if !source.contains("://") {
        let path = std::fs::canonicalize(source)
            .with_context(|| format!("cannot resolve dataset {source}"))?;
        let uri = path
            .to_str()
            .context("dataset path is not valid UTF-8")?
            .to_owned();
        return Ok((uri.clone(), DatasetBuilder::from_uri(&uri)));
    }

    let uri = Url::parse(source).context("invalid dataset URI")?;
    if uri.scheme() == "file" {
        let path = uri
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file URI"))?;
        return dataset_builder(path.to_str().context("dataset path is not valid UTF-8")?);
    }
    if uri.scheme() != "s3" {
        bail!(
            "unsupported dataset scheme {}; use s3:// or a local path",
            uri.scheme()
        );
    }
    let bucket = uri.host_str().context("S3 URI must include a bucket")?;
    // OpenDAL reads endpoint, region, and credentials from AWS_* variables.
    opendal::install_default();
    let operator = Operator::new(services::S3::default().bucket(bucket))?;
    let session = Arc::new(Session::default());
    session.store_registry().insert(
        "s3",
        Arc::new(S3Provider(Arc::new(OpendalStore::new(operator)))),
    );
    Ok((
        source.trim_end_matches('/').to_owned(),
        DatasetBuilder::from_uri(source).with_session(session),
    ))
}
