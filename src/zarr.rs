use std::{collections::HashSet, iter, num::NonZeroU64, path::PathBuf, sync::Arc};

use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, TryFromInto};
use thiserror::Error;
use tokio::spawn;

use crate::{
    dataset::{
        ArrayShape, ChunkIndices, ChunkKeyEncoding, ChunkShape, Codec, DataType,
        DatasetError, DimensionNames, FillValue, Path, StorageTransformer,
        UserAttributes, ZarrArrayMetadata,
    },
    format::{
        structure::{NodeData, UserAttributesStructure}, // TODO: we shouldn't need these imports, too low level
        ChunkOffset,
        IcechunkFormatError,
    },
    Dataset, Storage,
};

pub use crate::format::ObjectId;
pub use crate::format::SnapshotId;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StorageConfig {
    #[serde(rename = "in_memory")]
    InMemory,

    #[serde(rename = "local_filesystem")]
    LocalFileSystem { root: PathBuf },

    #[serde(rename = "cached")]
    Cached { approx_max_memory_bytes: u64, backend: Box<StorageConfig> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VersionInfo {
    #[serde(rename = "empty")]
    Empty,

    #[serde(rename = "structure_id")]
    StructureId(ObjectId),

    #[serde(rename = "snapshot_id")]
    SnapshotId(SnapshotId), //TODO: unimplemented yet
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetConfig {
    pub previous_version: VersionInfo,

    pub inline_chunk_threshold_bytes: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoreConfig {
    storage: StorageConfig,
    dataset: DatasetConfig,
}

pub type ByteRange = (Option<ChunkOffset>, Option<ChunkOffset>);
pub type StoreResult<A> = Result<A, StoreError>;

#[derive(Debug, Clone)]
pub struct Store {
    dataset: Dataset,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyNotFoundError {
    #[error("chunk cannot be find for key `{key}`")]
    ChunkNotFound { key: String, path: Path, coords: ChunkIndices },
    #[error("node not found at `{path}`")]
    NodeNotFound { path: Path },
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invalid zarr key format `{key}`")]
    InvalidKey { key: String },
    #[error("object not found: `{0}`")]
    NotFound(#[from] KeyNotFoundError),
    #[error("unsuccessful dataset operation: `{0}`")]
    CannotUpdate(#[from] DatasetError),
    #[error("bad metadata: `{0}`")]
    BadMetadata(#[from] serde_json::Error),
    #[error("store method `{0}` is not implemented by Icechunk")]
    Unimplemented(&'static str),
    #[error("bad key prefix: `{0}`")]
    BadKeyPrefix(String),
    #[error("unknown store error: `{0}`")]
    Unknown(Box<dyn std::error::Error + Send + Sync>),
}

impl Store {
    pub fn from_config(config: &StoreConfig) -> Result<Self, String> {
        let storage = mk_storage(&config.storage)?;
        let dataset = mk_dataset(&config.dataset, storage)?;
        Ok(Self::new(dataset))
    }

    pub fn from_json_config(json: &[u8]) -> Result<Self, String> {
        let config: StoreConfig =
            serde_json::from_slice(json).map_err(|e| e.to_string())?;
        Self::from_config(&config)
    }

    pub fn new(dataset: Dataset) -> Self {
        Store { dataset }
    }

    pub fn dataset(self) -> Dataset {
        self.dataset
    }

    pub async fn empty(&self) -> StoreResult<bool> {
        let res = self.dataset.list_nodes().await?.next().is_none();
        Ok(res)
    }

    pub async fn clear(&mut self) -> StoreResult<()> {
        todo!()
    }

    // TODO: prototype argument
    pub async fn get(&self, key: &str, _byte_range: &ByteRange) -> StoreResult<Bytes> {
        match Key::parse(key)? {
            Key::Metadata { node_path } => self.get_metadata(key, &node_path).await,
            Key::Chunk { node_path, coords } => {
                self.get_chunk(key, node_path, coords).await
            }
        }
    }

    // TODO: prototype argument
    pub async fn get_partial_values(
        // We need an Arc here because otherwise we cannot spawn concurrent tasks
        self: Arc<Self>,
        key_ranges: impl IntoIterator<Item = (String, ByteRange)>,
    ) -> StoreResult<Vec<StoreResult<Bytes>>> {
        let mut tasks = Vec::new();
        for (key, range) in key_ranges {
            let this = Arc::clone(&self);
            tasks.push(spawn(async move { this.get(&key, &range).await }));
        }
        let mut outputs = Vec::with_capacity(tasks.len());
        for task in tasks {
            outputs.push(task.await);
        }
        outputs.into_iter().try_collect().map_err(|e| StoreError::Unknown(Box::new(e)))
    }

    // TODO: prototype argument
    pub async fn exists(&self, key: &str) -> StoreResult<bool> {
        match self.get(key, &(None, None)).await {
            Ok(_) => Ok(true),
            Err(StoreError::NotFound(_)) => Ok(false),
            Err(other_error) => Err(other_error),
        }
    }

    pub fn supports_writes(&self) -> StoreResult<bool> {
        Ok(true)
    }

    pub async fn set(&mut self, key: &str, value: Bytes) -> StoreResult<()> {
        match Key::parse(key)? {
            Key::Metadata { node_path } => {
                if let Ok(array_meta) = serde_json::from_slice(value.as_ref()) {
                    self.set_array_meta(node_path, array_meta).await
                } else {
                    match serde_json::from_slice(value.as_ref()) {
                        Ok(group_meta) => {
                            self.set_group_meta(node_path, group_meta).await
                        }
                        Err(err) => Err(StoreError::BadMetadata(err)),
                    }
                }
            }
            Key::Chunk { ref node_path, ref coords } => {
                self.dataset.set_chunk(node_path, coords, value).await?;
                Ok(())
            }
        }
    }

    pub async fn delete(&mut self, key: &str) -> StoreResult<()> {
        let ds = &mut self.dataset;
        match Key::parse(key)? {
            Key::Metadata { node_path } => {
                let node = ds.get_node(&node_path).await.map_err(|_| {
                    KeyNotFoundError::NodeNotFound { path: node_path.clone() }
                })?;
                match node.node_data {
                    NodeData::Array(_, _) => Ok(ds.delete_array(node_path).await?),
                    NodeData::Group => Ok(ds.delete_group(node_path).await?),
                }
            }
            Key::Chunk { node_path, coords } => {
                Ok(ds.set_chunk_ref(node_path, coords, None).await?)
            }
        }
    }

    pub fn supports_partial_writes(&self) -> StoreResult<bool> {
        Ok(false)
    }

    pub async fn set_partial_values(
        &mut self,
        _key_start_values: impl IntoIterator<Item = (&str, ChunkOffset, Bytes)>,
    ) -> StoreResult<()> {
        Err(StoreError::Unimplemented("set_partial_values"))
    }

    pub fn supports_listing(&self) -> StoreResult<bool> {
        Ok(true)
    }

    pub async fn list(
        &self,
    ) -> StoreResult<impl Stream<Item = StoreResult<String>> + '_> {
        self.list_prefix("/").await
    }

    pub async fn list_prefix<'a>(
        &'a self,
        prefix: &'a str,
        // TODO: item should probably be StoreResult<String>
    ) -> StoreResult<impl Stream<Item = StoreResult<String>> + 'a> {
        // TODO: this is inefficient because it filters based on the prefix, instead of only
        // generating items that could potentially match
        let meta = self.list_metadata_prefix(prefix).await?;
        let chunks = self.list_chunks_prefix(prefix).await?;
        Ok(meta.chain(chunks))
    }

    pub async fn list_dir<'a>(
        &'a self,
        prefix: &'a str,
    ) -> StoreResult<impl Stream<Item = StoreResult<String>> + 'a> {
        // TODO: this is inefficient because it filters based on the prefix, instead of only
        // generating items that could potentially match
        // FIXME: this is not lazy, it goes through every chunk. This should be implemented using
        // metadata only, and ignore the chunks, but we should decide on that based on Zarr3 spec
        // evolution

        let idx = if prefix == "/" { 0 } else { prefix.len() };

        let parents: HashSet<_> = self
            .list_prefix(prefix)
            .await?
            .map_ok(move |s| {
                let rem = &s[idx..];
                let parent = rem.split_once('/').map_or(rem, |(parent, _)| parent);
                parent.to_string()
            })
            .try_collect()
            .await?;
        // We tould return a Stream<Item = String> with this implementation, but the present
        // signature is better if we change the impl
        Ok(futures::stream::iter(parents.into_iter().map(Ok)))
    }

    async fn get_chunk(
        &self,
        key: &str,
        path: Path,
        coords: ChunkIndices,
    ) -> StoreResult<Bytes> {
        let chunk = self.dataset.get_chunk(&path, &coords).await?;
        chunk.ok_or(StoreError::NotFound(KeyNotFoundError::ChunkNotFound {
            key: key.to_string(),
            path,
            coords,
        }))
    }

    async fn get_metadata(&self, _key: &str, path: &Path) -> StoreResult<Bytes> {
        let node = self.dataset.get_node(path).await.map_err(|_| {
            StoreError::NotFound(KeyNotFoundError::NodeNotFound { path: path.clone() })
        })?;
        let user_attributes = match node.user_attributes {
            None => None,
            Some(UserAttributesStructure::Inline(atts)) => Some(atts),
            // FIXME: implement
            Some(UserAttributesStructure::Ref(_)) => todo!(),
        };
        match node.node_data {
            NodeData::Group => Ok(GroupMetadata::new(user_attributes).to_bytes()),
            NodeData::Array(zarr_metadata, _) => {
                Ok(ArrayMetadata::new(user_attributes, zarr_metadata).to_bytes())
            }
        }
    }

    async fn set_array_meta(
        &mut self,
        path: Path,
        array_meta: ArrayMetadata,
    ) -> Result<(), StoreError> {
        if self.dataset.get_array(&path).await.is_ok() {
            // TODO: we don't necessarily need to update both
            self.dataset.set_user_attributes(path.clone(), array_meta.attributes).await?;
            self.dataset.update_array(path, array_meta.zarr_metadata).await?;
            Ok(())
        } else {
            self.dataset.add_array(path.clone(), array_meta.zarr_metadata).await?;
            self.dataset.set_user_attributes(path, array_meta.attributes).await?;
            Ok(())
        }
    }

    async fn set_group_meta(
        &mut self,
        path: Path,
        group_meta: GroupMetadata,
    ) -> Result<(), StoreError> {
        if self.dataset.get_group(&path).await.is_ok() {
            self.dataset.set_user_attributes(path, group_meta.attributes).await?;
            Ok(())
        } else {
            self.dataset.add_group(path.clone()).await?;
            self.dataset.set_user_attributes(path, group_meta.attributes).await?;
            Ok(())
        }
    }

    async fn list_metadata_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> StoreResult<impl Stream<Item = StoreResult<String>> + 'a> {
        if let Some(prefix) = prefix.strip_suffix('/') {
            let nodes = futures::stream::iter(self.dataset.list_nodes().await?);
            // TODO: handle non-utf8?
            Ok(nodes.map_err(|e| e.into()).try_filter_map(move |node| async move {
                Ok(Key::Metadata { node_path: node.path }.to_string().and_then(|key| {
                    if key.starts_with(prefix) {
                        Some(key)
                    } else {
                        None
                    }
                }))
            }))
        } else {
            Err(StoreError::BadKeyPrefix(prefix.to_string()))
        }
    }

    async fn list_chunks_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> StoreResult<impl Stream<Item = StoreResult<String>> + 'a> {
        // TODO: this is inefficient because it filters based on the prefix, instead of only
        // generating items that could potentially match
        if let Some(prefix) = prefix.strip_suffix('/') {
            let chunks = self.dataset.all_chunks().await?;
            Ok(chunks.map_err(|e| e.into()).try_filter_map(
                move |(path, chunk)| async move {
                    //FIXME: utf handling
                    Ok(Key::Chunk { node_path: path, coords: chunk.coord }
                        .to_string()
                        .and_then(
                            |key| if key.starts_with(prefix) { Some(key) } else { None },
                        ))
                },
            ))
        } else {
            Err(StoreError::BadKeyPrefix(prefix.to_string()))
        }
    }
}

fn mk_dataset(
    _dataset: &DatasetConfig,
    _storage: Arc<dyn Storage + Send + Sync>,
) -> Result<Dataset, String> {
    todo!()
}

fn mk_storage(_config: &StorageConfig) -> Result<Arc<dyn Storage + Send + Sync>, String> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    Metadata { node_path: Path },
    Chunk { node_path: Path, coords: ChunkIndices },
}

impl Key {
    const ROOT_KEY: &'static str = "zarr.json";
    const METADATA_SUFFIX: &'static str = "/zarr.json";
    const CHUNK_COORD_INFIX: &'static str = "/c";

    fn parse(key: &str) -> Result<Self, StoreError> {
        fn parse_chunk(key: &str) -> Result<Key, StoreError> {
            if key == "c" {
                return Ok(Key::Chunk {
                    node_path: "/".into(),
                    coords: ChunkIndices(vec![]),
                });
            }
            if let Some((path, coords)) = key.rsplit_once(Key::CHUNK_COORD_INFIX) {
                if coords.is_empty() {
                    Ok(Key::Chunk {
                        node_path: ["/", path].iter().collect(),
                        coords: ChunkIndices(vec![]),
                    })
                } else {
                    coords
                        .strip_prefix('/')
                        .ok_or(StoreError::InvalidKey { key: key.to_string() })?
                        .split('/')
                        .map(|s| s.parse::<u64>())
                        .collect::<Result<Vec<_>, _>>()
                        .map(|coords| Key::Chunk {
                            node_path: ["/", path].iter().collect(),
                            coords: ChunkIndices(coords),
                        })
                        .map_err(|_| StoreError::InvalidKey { key: key.to_string() })
                }
            } else {
                Err(StoreError::InvalidKey { key: key.to_string() })
            }
        }

        if key == Key::ROOT_KEY {
            Ok(Key::Metadata { node_path: "/".into() })
        } else if key.starts_with('/') {
            Err(StoreError::InvalidKey { key: key.to_string() })
        } else if let Some(path) = key.strip_suffix(Key::METADATA_SUFFIX) {
            // we need to be careful indexing into utf8 strings
            Ok(Key::Metadata { node_path: ["/", path].iter().collect() })
        } else {
            parse_chunk(key)
        }
    }

    fn to_string(&self) -> Option<String> {
        match self {
            Key::Metadata { node_path } => node_path.as_path().to_str().map(|s| {
                format!("{}{}", &s[1..], Key::METADATA_SUFFIX)
                    .trim_start_matches('/')
                    .to_string()
            }),
            Key::Chunk { node_path, coords } => {
                node_path.as_path().to_str().map(|path| {
                    let coords = coords.0.iter().map(|c| c.to_string()).join("/");
                    [path[1..].to_string(), "c".to_string(), coords]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .join("/")
                })
            }
        }
    }
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
struct ArrayMetadata {
    zarr_format: u8,
    node_type: String,
    attributes: Option<UserAttributes>,
    #[serde(flatten)]
    #[serde_as(as = "TryFromInto<ZarrArrayMetadataSerialzer>")]
    zarr_metadata: ZarrArrayMetadata,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct ZarrArrayMetadataSerialzer {
    pub shape: ArrayShape,
    pub data_type: DataType,

    #[serde_as(as = "TryFromInto<NameConfigSerializer>")]
    #[serde(rename = "chunk_grid")]
    pub chunk_shape: ChunkShape,

    #[serde_as(as = "TryFromInto<NameConfigSerializer>")]
    pub chunk_key_encoding: ChunkKeyEncoding,
    pub fill_value: serde_json::Value,
    pub codecs: Vec<Codec>,
    pub storage_transformers: Option<Vec<StorageTransformer>>,
    // each dimension name can be null in Zarr
    pub dimension_names: Option<DimensionNames>,
}

impl TryFrom<ZarrArrayMetadataSerialzer> for ZarrArrayMetadata {
    type Error = IcechunkFormatError;

    fn try_from(value: ZarrArrayMetadataSerialzer) -> Result<Self, Self::Error> {
        let ZarrArrayMetadataSerialzer {
            shape,
            data_type,
            chunk_shape,
            chunk_key_encoding,
            fill_value,
            codecs,
            storage_transformers,
            dimension_names,
        } = value;
        {
            let fill_value = FillValue::from_data_type_and_json(&data_type, &fill_value)?;
            Ok(ZarrArrayMetadata {
                fill_value,
                shape,
                data_type,
                chunk_shape,
                chunk_key_encoding,
                codecs,
                storage_transformers,
                dimension_names,
            })
        }
    }
}

impl From<ZarrArrayMetadata> for ZarrArrayMetadataSerialzer {
    fn from(value: ZarrArrayMetadata) -> Self {
        let ZarrArrayMetadata {
            shape,
            data_type,
            chunk_shape,
            chunk_key_encoding,
            fill_value,
            codecs,
            storage_transformers,
            dimension_names,
        } = value;
        {
            #[allow(clippy::expect_used)]
            let fill_value = serde_json::to_value(fill_value)
                .expect("Fill values are always serializable");
            ZarrArrayMetadataSerialzer {
                shape,
                data_type,
                chunk_shape,
                chunk_key_encoding,
                codecs,
                storage_transformers,
                dimension_names,
                fill_value,
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GroupMetadata {
    zarr_format: u8,
    node_type: String,
    attributes: Option<UserAttributes>,
}

impl ArrayMetadata {
    fn new(attributes: Option<UserAttributes>, zarr_metadata: ZarrArrayMetadata) -> Self {
        Self { zarr_format: 3, node_type: "array".to_string(), attributes, zarr_metadata }
    }

    fn to_bytes(&self) -> Bytes {
        Bytes::from_iter(
            // We can unpack because it comes from controlled datastructures that can be serialized
            #[allow(clippy::expect_used)]
            serde_json::to_vec(self).expect("bug in ArrayMetadata serialization"),
        )
    }
}

impl GroupMetadata {
    fn new(attributes: Option<UserAttributes>) -> Self {
        Self { zarr_format: 3, node_type: "group".to_string(), attributes }
    }

    fn to_bytes(&self) -> Bytes {
        Bytes::from_iter(
            // We can unpack because it comes from controlled datastructures that can be serialized
            #[allow(clippy::expect_used)]
            serde_json::to_vec(self).expect("bug in GroupMetadata serialization"),
        )
    }
}

#[derive(Serialize, Deserialize)]
struct NameConfigSerializer {
    name: String,
    configuration: serde_json::Value,
}

impl From<ChunkShape> for NameConfigSerializer {
    fn from(value: ChunkShape) -> Self {
        let arr = serde_json::Value::Array(
            value
                .0
                .iter()
                .map(|v| {
                    serde_json::Value::Number(serde_json::value::Number::from(v.get()))
                })
                .collect(),
        );
        let kvs = serde_json::value::Map::from_iter(iter::once((
            "chunk_shape".to_string(),
            arr,
        )));
        Self {
            name: "regular".to_string(),
            configuration: serde_json::Value::Object(kvs),
        }
    }
}

impl TryFrom<NameConfigSerializer> for ChunkShape {
    type Error = &'static str;

    fn try_from(value: NameConfigSerializer) -> Result<Self, Self::Error> {
        match value {
            NameConfigSerializer {
                name,
                configuration: serde_json::Value::Object(kvs),
            } if name == "regular" => {
                let values = kvs
                    .get("chunk_shape")
                    .and_then(|v| v.as_array())
                    .ok_or("cannot parse ChunkShape")?;
                let shape = values
                    .iter()
                    .map(|v| v.as_u64().and_then(|u64| NonZeroU64::try_from(u64).ok()))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("cannot parse ChunkShape")?;
                Ok(ChunkShape(shape))
            }
            _ => Err("cannot parse ChunkShape"),
        }
    }
}

impl From<ChunkKeyEncoding> for NameConfigSerializer {
    fn from(_value: ChunkKeyEncoding) -> Self {
        let kvs = serde_json::value::Map::from_iter(iter::once((
            "separator".to_string(),
            serde_json::Value::String("/".to_string()),
        )));
        Self {
            name: "default".to_string(),
            configuration: serde_json::Value::Object(kvs),
        }
    }
}

impl TryFrom<NameConfigSerializer> for ChunkKeyEncoding {
    type Error = &'static str;

    fn try_from(value: NameConfigSerializer) -> Result<Self, Self::Error> {
        //FIXME: we are hardcoding / as the separator
        match value {
            NameConfigSerializer {
                name,
                configuration: serde_json::Value::Object(kvs),
            } if name == "default" => {
                if let Some("/") =
                    kvs.get("separator").ok_or("cannot parse ChunkKeyEncoding")?.as_str()
                {
                    Ok(ChunkKeyEncoding::Slash)
                } else {
                    Err("cannot parse ChunkKeyEncoding")
                }
            }
            _ => Err("cannot parse ChunkKeyEncoding"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
mod tests {

    use std::borrow::BorrowMut;

    use crate::{storage::InMemoryStorage, Storage};

    use super::*;
    use pretty_assertions::assert_eq;

    async fn all_keys(store: &Store) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let version1 = keys(store, "/").await?;
        let mut version2 = store.list().await?.try_collect::<Vec<_>>().await?;
        version2.sort();
        assert_eq!(version1, version2);
        Ok(version1)
    }

    async fn keys(
        store: &Store,
        prefix: &str,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut res = store.list_prefix(prefix).await?.try_collect::<Vec<_>>().await?;
        res.sort();
        Ok(res)
    }

    #[test]
    fn test_parse_key() {
        assert!(matches!(
            Key::parse("zarr.json"),
            Ok(Key::Metadata { node_path}) if node_path.to_str() == Some("/")
        ));
        assert!(matches!(
            Key::parse("a/zarr.json"),
            Ok(Key::Metadata { node_path }) if node_path.to_str() == Some("/a")
        ));
        assert!(matches!(
            Key::parse("a/b/c/zarr.json"),
            Ok(Key::Metadata { node_path }) if node_path.to_str() == Some("/a/b/c")
        ));
        assert!(matches!(
            Key::parse("foo/c"),
            Ok(Key::Chunk { node_path, coords }) if node_path.to_str() == Some("/foo") && coords == ChunkIndices(vec![])
        ));
        assert!(matches!(
            Key::parse("foo/bar/c"),
            Ok(Key::Chunk { node_path, coords}) if node_path.to_str() == Some("/foo/bar") && coords == ChunkIndices(vec![])
        ));
        assert!(matches!(
            Key::parse("foo/c/1/2/3"),
            Ok(Key::Chunk {
                node_path,
                coords,
            }) if node_path.to_str() == Some("/foo") && coords == ChunkIndices(vec![1,2,3])
        ));
        assert!(matches!(
            Key::parse("foo/bar/baz/c/1/2/3"),
            Ok(Key::Chunk {
                node_path,
                coords,
            }) if node_path.to_str() == Some("/foo/bar/baz") && coords == ChunkIndices(vec![1,2,3])
        ));
        assert!(matches!(
            Key::parse("c"),
            Ok(Key::Chunk { node_path, coords}) if node_path.to_str() == Some("/") && coords == ChunkIndices(vec![])
        ));
    }

    #[test]
    fn test_format_key() {
        assert_eq!(
            Key::Metadata { node_path: "/".into() }.to_string(),
            Some("zarr.json".to_string())
        );
        assert_eq!(
            Key::Metadata { node_path: "/a".into() }.to_string(),
            Some("a/zarr.json".to_string())
        );
        assert_eq!(
            Key::Metadata { node_path: "/a/b/c".into() }.to_string(),
            Some("a/b/c/zarr.json".to_string())
        );
        assert_eq!(
            Key::Chunk { node_path: "/".into(), coords: ChunkIndices(vec![]) }
                .to_string(),
            Some("c".to_string())
        );
        assert_eq!(
            Key::Chunk { node_path: "/".into(), coords: ChunkIndices(vec![0]) }
                .to_string(),
            Some("c/0".to_string())
        );
        assert_eq!(
            Key::Chunk { node_path: "/".into(), coords: ChunkIndices(vec![1, 2]) }
                .to_string(),
            Some("c/1/2".to_string())
        );
        assert_eq!(
            Key::Chunk { node_path: "/a".into(), coords: ChunkIndices(vec![]) }
                .to_string(),
            Some("a/c".to_string())
        );
        assert_eq!(
            Key::Chunk { node_path: "/a".into(), coords: ChunkIndices(vec![1]) }
                .to_string(),
            Some("a/c/1".to_string())
        );
        assert_eq!(
            Key::Chunk { node_path: "/a".into(), coords: ChunkIndices(vec![1, 2]) }
                .to_string(),
            Some("a/c/1/2".to_string())
        );
    }

    #[tokio::test]
    async fn test_metadata_set_and_get() -> Result<(), Box<dyn std::error::Error>> {
        let storage: Arc<dyn Storage + Send + Sync> = Arc::new(InMemoryStorage::new());
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);

        assert!(matches!(
            store.get("zarr.json", &(None, None)).await,
            Err(StoreError::NotFound(KeyNotFoundError::NodeNotFound {path})) if path.to_str() == Some("/")
        ));

        store
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await?;
        assert_eq!(
            store.get("zarr.json", &(None, None)).await.unwrap(),
            Bytes::copy_from_slice(
                br#"{"zarr_format":3,"node_type":"group","attributes":null}"#
            )
        );

        store.set("a/b/zarr.json", Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group", "attributes": {"spam":"ham", "eggs":42}}"#)).await?;
        assert_eq!(
            store.get("a/b/zarr.json", &(None, None)).await.unwrap(),
            Bytes::copy_from_slice(
                br#"{"zarr_format":3,"node_type":"group","attributes":{"eggs":42,"spam":"ham"}}"#
            )
        );

        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("a/b/array/zarr.json", zarr_meta.clone()).await?;
        assert_eq!(
            store.get("a/b/array/zarr.json", &(None, None)).await.unwrap(),
            zarr_meta.clone()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_metadata_delete() {
        let in_mem_storage = Arc::new(InMemoryStorage::new());
        let storage =
            Arc::clone(&(in_mem_storage.clone() as Arc<dyn Storage + Send + Sync>));
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);
        let group_data = br#"{"zarr_format":3, "node_type":"group", "attributes": {"spam":"ham", "eggs":42}}"#;

        store
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await
            .unwrap();
        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("array/zarr.json", zarr_meta.clone()).await.unwrap();

        // delete metadata tests
        store.delete("array/zarr.json").await.unwrap();
        assert!(matches!(
            store.get("array/zarr.json", &(None, None)).await,
            Err(StoreError::NotFound(KeyNotFoundError::NodeNotFound { path }))
                if path.to_str() == Some("/array"),
        ));
        store.set("array/zarr.json", zarr_meta.clone()).await.unwrap();
        store.delete("array/zarr.json").await.unwrap();
        assert!(matches!(
            store.get("array/zarr.json", &(None, None)).await,
            Err(StoreError::NotFound(KeyNotFoundError::NodeNotFound { path } ))
                if path.to_str() == Some("/array"),
        ));
        store.set("array/zarr.json", Bytes::copy_from_slice(group_data)).await.unwrap();
    }

    #[tokio::test]
    async fn test_chunk_set_and_get() -> Result<(), Box<dyn std::error::Error>> {
        // TODO: turn this test into pure Store operations once we support writes through Zarr
        let in_mem_storage = Arc::new(InMemoryStorage::new());
        let storage =
            Arc::clone(&(in_mem_storage.clone() as Arc<dyn Storage + Send + Sync>));
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);

        store
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await?;
        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("array/zarr.json", zarr_meta.clone()).await?;

        // a small inline chunk
        let small_data = Bytes::copy_from_slice(b"hello");
        store.set("array/c/0/1/0", small_data.clone()).await?;
        assert_eq!(store.get("array/c/0/1/0", &(None, None)).await.unwrap(), small_data);
        // no new chunks written because it was inline
        assert!(in_mem_storage.chunk_ids().is_empty());

        // a big chunk
        let big_data = Bytes::copy_from_slice(b"hello".repeat(512).as_slice());
        store.set("array/c/0/1/1", big_data.clone()).await?;
        assert_eq!(store.get("array/c/0/1/1", &(None, None)).await.unwrap(), big_data);
        let chunk_id = in_mem_storage.chunk_ids().iter().next().cloned().unwrap();
        assert_eq!(in_mem_storage.fetch_chunk(&chunk_id, &None).await?, big_data);

        let mut ds = store.dataset();
        let oid = ds.flush().await?;

        let ds = Dataset::update(storage, oid).build();
        let store = Store::new(ds);
        assert_eq!(store.get("array/c/0/1/0", &(None, None)).await.unwrap(), small_data);
        assert_eq!(store.get("array/c/0/1/1", &(None, None)).await.unwrap(), big_data);

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_delete() {
        let in_mem_storage = Arc::new(InMemoryStorage::new());
        let storage =
            Arc::clone(&(in_mem_storage.clone() as Arc<dyn Storage + Send + Sync>));
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);

        store
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await
            .unwrap();
        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("array/zarr.json", zarr_meta.clone()).await.unwrap();

        let data = Bytes::copy_from_slice(b"hello");
        store.set("array/c/0/1/0", data.clone()).await.unwrap();

        // delete chunk
        store.delete("array/c/0/1/0").await.unwrap();
        // deleting a deleted chunk is allowed
        store.delete("array/c/0/1/0").await.unwrap();
        // deleting non-existent chunk is allowed
        store.delete("array/c/1/1/1").await.unwrap();
        assert!(matches!(
            store.get("array/c/0/1/0", &(None, None)).await,
            Err(StoreError::NotFound(KeyNotFoundError::ChunkNotFound { key, path, coords }))
                if key == "array/c/0/1/0" && path.to_str() == Some("/array") && coords == ChunkIndices([0, 1, 0].to_vec())
        ));
        assert!(matches!(
            store.delete("array/foo").await,
            Err(StoreError::InvalidKey { key }) if key == "array/foo",
        ));
        // FIXME: deleting an invalid chunk should not be allowed.
        store.delete("array/c/10/1/1").await.unwrap();
    }

    #[tokio::test]
    async fn test_metadata_list() -> Result<(), Box<dyn std::error::Error>> {
        let storage: Arc<dyn Storage + Send + Sync> = Arc::new(InMemoryStorage::new());
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);

        assert!(
            matches!(store.list_prefix("").await, Err(StoreError::BadKeyPrefix(p)) if p.is_empty())
        );
        assert!(
            matches!(store.list_prefix("foo").await, Err(StoreError::BadKeyPrefix(p)) if p == "foo")
        );
        assert!(
            matches!(store.list_prefix("foo/bar").await, Err(StoreError::BadKeyPrefix(p)) if p == "foo/bar")
        );
        assert!(
            matches!(store.list_prefix("/foo/bar").await, Err(StoreError::BadKeyPrefix(p)) if p == "/foo/bar")
        );

        assert!(store.empty().await.unwrap());
        assert!(!store.exists("zarr.json").await.unwrap());

        assert_eq!(all_keys(&store).await.unwrap(), Vec::<String>::new());
        store
            .borrow_mut()
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await?;

        assert!(!store.empty().await.unwrap());
        assert!(store.exists("zarr.json").await.unwrap());
        assert_eq!(all_keys(&store).await.unwrap(), vec!["zarr.json".to_string()]);
        store
            .borrow_mut()
            .set(
                "group/zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await?;
        assert_eq!(
            all_keys(&store).await.unwrap(),
            vec!["group/zarr.json".to_string(), "zarr.json".to_string()]
        );
        assert_eq!(
            keys(&store, "group/").await.unwrap(),
            vec!["group/zarr.json".to_string()]
        );

        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("group/array/zarr.json", zarr_meta).await?;
        assert!(!store.empty().await.unwrap());
        assert!(store.exists("zarr.json").await.unwrap());
        assert!(store.exists("group/array/zarr.json").await.unwrap());
        assert!(store.exists("group/zarr.json").await.unwrap());
        assert_eq!(
            all_keys(&store).await.unwrap(),
            vec![
                "group/array/zarr.json".to_string(),
                "group/zarr.json".to_string(),
                "zarr.json".to_string()
            ]
        );
        assert_eq!(
            keys(&store, "group/").await.unwrap(),
            vec!["group/array/zarr.json".to_string(), "group/zarr.json".to_string()]
        );
        assert_eq!(
            keys(&store, "group/array/").await.unwrap(),
            vec!["group/array/zarr.json".to_string()]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_list() -> Result<(), Box<dyn std::error::Error>> {
        let storage: Arc<dyn Storage + Send + Sync> = Arc::new(InMemoryStorage::new());
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);

        store
            .borrow_mut()
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await?;

        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("array/zarr.json", zarr_meta).await?;

        let data = Bytes::copy_from_slice(b"hello");
        store.set("array/c/0/1/0", data.clone()).await?;
        store.set("array/c/1/1/1", data.clone()).await?;

        assert_eq!(
            all_keys(&store).await.unwrap(),
            vec![
                "array/c/0/1/0".to_string(),
                "array/c/1/1/1".to_string(),
                "array/zarr.json".to_string(),
                "zarr.json".to_string()
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_list_dir() -> Result<(), Box<dyn std::error::Error>> {
        let storage: Arc<dyn Storage + Send + Sync> = Arc::new(InMemoryStorage::new());
        let ds = Dataset::create(Arc::clone(&storage)).build();
        let mut store = Store::new(ds);

        store
            .borrow_mut()
            .set(
                "zarr.json",
                Bytes::copy_from_slice(br#"{"zarr_format":3, "node_type":"group"}"#),
            )
            .await?;

        let zarr_meta = Bytes::copy_from_slice(br#"{"zarr_format":3,"node_type":"array","attributes":{"foo":42},"shape":[2,2,2],"data_type":"int32","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,1,1]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"mycodec","configuration":{"foo":42}}],"storage_transformers":[{"name":"mytransformer","configuration":{"bar":43}}],"dimension_names":["x","y","t"]}"#);
        store.set("array/zarr.json", zarr_meta).await?;

        let data = Bytes::copy_from_slice(b"hello");
        store.set("array/c/0/1/0", data.clone()).await?;
        store.set("array/c/1/1/1", data.clone()).await?;

        assert_eq!(
            all_keys(&store).await.unwrap(),
            vec![
                "array/c/0/1/0".to_string(),
                "array/c/1/1/1".to_string(),
                "array/zarr.json".to_string(),
                "zarr.json".to_string()
            ]
        );

        let mut dir = store.list_dir("/").await?.try_collect::<Vec<_>>().await?;
        dir.sort();
        assert_eq!(dir, vec!["array".to_string(), "zarr.json".to_string()]);

        let mut dir = store.list_dir("array/").await?.try_collect::<Vec<_>>().await?;
        dir.sort();
        assert_eq!(dir, vec!["c".to_string(), "zarr.json".to_string()]);

        let mut dir = store.list_dir("array/c/").await?.try_collect::<Vec<_>>().await?;
        dir.sort();
        assert_eq!(dir, vec!["0".to_string(), "1".to_string()]);

        let mut dir = store.list_dir("array/c/1/").await?.try_collect::<Vec<_>>().await?;
        dir.sort();
        assert_eq!(dir, vec!["1".to_string()]);
        Ok(())
    }

    #[test]
    fn test_store_config_deserialization() -> Result<(), Box<dyn std::error::Error>> {
        let expected = StoreConfig {
            storage: StorageConfig::Cached {
                approx_max_memory_bytes: 1_000_000,
                backend: Box::new(StorageConfig::LocalFileSystem {
                    root: "/tmp/test".into(),
                }),
            },
            dataset: DatasetConfig {
                inline_chunk_threshold_bytes: Some(128),
                previous_version: VersionInfo::StructureId(ObjectId([
                    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
                ])),
            },
        };

        let json = r#"
            {"storage":
                {"cached":{
                    "approx_max_memory_bytes":1000000,
                    "backend":{"local_filesystem":{"root":"/tmp/test"}}
                }},
             "dataset": {
                "previous_version":{"structure_id":"000102030405060708090a0b0c0d0e0f"},
                "inline_chunk_threshold_bytes":128
            }}
        "#;
        //let json = serde_json::to_string(&value)?;
        let config: StoreConfig = serde_json::from_str(json)?;
        assert_eq!(expected, config);
        Ok(())
    }
}
