//! Open remote datasets through OpenDAL's object_store adapter.

use std::{collections::HashMap, sync::Arc};

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
struct OpenDalProvider {
    store: Arc<dyn object_store::ObjectStore>,
}

#[async_trait::async_trait]
impl ObjectStoreProvider for OpenDalProvider {
    async fn new_store(
        &self,
        location: Url,
        params: &ObjectStoreParams,
    ) -> lance_core::Result<ObjectStore> {
        Ok(ObjectStore::new(
            self.store.clone(),
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
    if !uri.username().is_empty()
        || uri.password().is_some()
        || uri.query().is_some()
        || uri.fragment().is_some()
    {
        bail!(
            "dataset URI must not contain credentials, query parameters, or fragments; use environment variables"
        );
    }
    if uri.scheme() == "file" {
        let path = uri
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file URI"))?;
        return dataset_builder(path.to_str().context("dataset path is not valid UTF-8")?);
    }
    let bucket = uri
        .host_str()
        .context("dataset URI must include a bucket or container")?;
    if uri.port().is_some() {
        bail!("configure the endpoint through environment variables");
    }
    let backend = match uri.scheme() {
        "s3" => "S3",
        "gs" => "GCS",
        "az" => "AZBLOB",
        scheme => {
            bail!("unsupported dataset scheme {scheme}; use s3://, gs://, az://, or a local path")
        }
    };
    // Backend options come from OPENDAL_<BACKEND>_<OPTION>.
    let prefix = format!("OPENDAL_{backend}_");
    let mut options: HashMap<String, String> = std::env::vars()
        .filter_map(|(key, value)| {
            key.strip_prefix(&prefix)
                .map(|key| (key.to_ascii_lowercase(), value))
        })
        .collect();
    // The URI always determines the location, even if these options exist in env.
    options.remove("bucket");
    options.remove("container");
    // Keep the operator rooted at the bucket: Lance passes bucket-relative paths.
    options.insert("root".into(), "/".into());
    opendal::install_default();
    let operator = match uri.scheme() {
        "s3" => {
            options.insert("bucket".into(), bucket.into());
            Operator::from_iter::<services::S3>(options)?
        }
        "gs" => {
            options.insert("bucket".into(), bucket.into());
            Operator::from_iter::<services::Gcs>(options)?
        }
        "az" => {
            options.insert("container".into(), bucket.into());
            Operator::from_iter::<services::Azblob>(options)?
        }
        scheme => {
            bail!("unsupported dataset scheme {scheme}; use s3://, gs://, az://, or a local path")
        }
    };
    let session = Arc::new(Session::default());
    session.store_registry().insert(
        uri.scheme(),
        Arc::new(OpenDalProvider {
            store: Arc::new(OpendalStore::new(operator)),
        }),
    );
    Ok((
        source.trim_end_matches('/').to_owned(),
        DatasetBuilder::from_uri(source).with_session(session),
    ))
}
