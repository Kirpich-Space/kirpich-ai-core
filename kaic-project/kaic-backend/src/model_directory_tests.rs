use super::*;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("kaic-model-directory-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn root(&self) -> PathBuf {
        self.0.join("models")
    }

    fn directory(&self) -> ModelDirectory {
        ModelDirectory::prepare(self.root()).unwrap()
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn header(tensors: u64, metadata: u64) -> Vec<u8> {
    let mut bytes = b"GGUF".to_vec();
    bytes.extend_from_slice(&3_u32.to_le_bytes());
    bytes.extend_from_slice(&tensors.to_le_bytes());
    bytes.extend_from_slice(&metadata.to_le_bytes());
    bytes
}

struct Gguf {
    bytes: Vec<u8>,
    tensor_start: usize,
    payload_start: usize,
}

impl Gguf {
    // GGUF v3 little endian, as written by upstream gguf_writer.py: one
    // F32 tensor with dimensions [1, 1], offset zero and 32-byte alignment.
    // This checks a real weight payload rather than a tokenizer-only header.
    fn new(tensors: bool, split_count: Option<u16>) -> Self {
        let mut bytes = header(u64::from(tensors), 2 + u64::from(split_count.is_some()));
        string(&mut bytes, "general.architecture");
        bytes.extend_from_slice(&8_u32.to_le_bytes()); // STRING
        string(&mut bytes, "llama");
        string(&mut bytes, "general.file_type");
        bytes.extend_from_slice(&4_u32.to_le_bytes()); // UINT32
        bytes.extend_from_slice(&0_u32.to_le_bytes()); // ALL_F32
        if let Some(count) = split_count {
            string(&mut bytes, "split.count");
            bytes.extend_from_slice(&2_u32.to_le_bytes()); // UINT16 upstream split count
            bytes.extend_from_slice(&count.to_le_bytes());
        }
        let tensor_start = bytes.len();
        if tensors {
            string(&mut bytes, "weight");
            bytes.extend_from_slice(&2_u32.to_le_bytes());
            bytes.extend_from_slice(&1_u64.to_le_bytes());
            bytes.extend_from_slice(&1_u64.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes()); // GGML_TYPE_F32
            bytes.extend_from_slice(&0_u64.to_le_bytes());
        }
        bytes.resize(bytes.len().div_ceil(32) * 32, 0);
        let payload_start = bytes.len();
        if tensors {
            bytes.extend_from_slice(&1_f32.to_le_bytes());
            bytes.resize(bytes.len().div_ceil(32) * 32, 0);
        }
        Self {
            bytes,
            tensor_start,
            payload_start,
        }
    }

    fn valid() -> Self {
        Self::new(true, None)
    }

    fn u32(&mut self, position: usize, value: u32) {
        self.bytes[position..position + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, position: usize, value: u64) {
        self.bytes[position..position + 8].copy_from_slice(&value.to_le_bytes());
    }
}

fn write(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn assert_rejected(bytes: &[u8]) {
    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let invalid = write(&fixture.root(), "invalid.gguf", bytes);
    let good = write(&fixture.root(), "valid.gguf", &Gguf::valid().bytes);
    let catalog = directory.scan().unwrap();
    assert_eq!(catalog.models.len(), 1, "invalid candidate was admitted");
    assert_eq!(catalog.models[0].path, good);
    assert!(
        catalog
            .rejected
            .iter()
            .any(|entry| entry.path == invalid && !entry.error.is_empty()),
        "invalid candidate must retain its path and diagnostic"
    );
}

struct Tensor<'a> {
    name: &'a str,
    dimensions: &'a [u64],
    kind: u32,
    offset: u64,
}

fn tensor_file(
    metadata: &[u8],
    metadata_count: u64,
    tensors: &[Tensor<'_>],
    payload_bytes: usize,
) -> Vec<u8> {
    let mut bytes = header(tensors.len() as u64, metadata_count);
    bytes.extend_from_slice(metadata);
    for tensor in tensors {
        string(&mut bytes, tensor.name);
        bytes.extend_from_slice(&(tensor.dimensions.len() as u32).to_le_bytes());
        for extent in tensor.dimensions {
            bytes.extend_from_slice(&extent.to_le_bytes());
        }
        bytes.extend_from_slice(&tensor.kind.to_le_bytes());
        bytes.extend_from_slice(&tensor.offset.to_le_bytes());
    }
    bytes.resize(bytes.len().div_ceil(32) * 32, 0);
    bytes.resize(bytes.len() + payload_bytes, 0);
    bytes
}

fn assert_discovered(bytes: &[u8]) -> LocalCatalog {
    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    write(&fixture.root(), "valid.gguf", bytes);
    let catalog = directory.scan().unwrap();
    assert_eq!(catalog.models.len(), 1, "{:?}", catalog.rejected);
    assert!(catalog.rejected.is_empty());
    catalog
}

#[test]
fn default_directory_is_lowercase_models_under_the_supplied_home() {
    let fixture = FixtureDir::new();
    let resolved = ModelDirectory::resolve(None, Some(fixture.0.as_os_str())).unwrap();
    assert_eq!(resolved, fixture.0.join("models"));
    assert_ne!(resolved, fixture.0.join("Models"));
    assert!(
        !resolved.exists(),
        "resolution alone must not create a directory"
    );
}

#[test]
fn explicit_directory_overrides_home_and_works_without_home() {
    let fixture = FixtureDir::new();
    let explicit = fixture.0.join("chosen weights");
    for home in [None, Some(OsStr::new("/irrelevant-home"))] {
        assert_eq!(
            ModelDirectory::resolve(Some(explicit.as_os_str()), home).unwrap(),
            explicit
        );
    }
}

#[test]
fn missing_home_without_an_explicit_directory_is_an_error() {
    assert!(ModelDirectory::resolve(None, None).is_err());
    assert!(ModelDirectory::resolve(None, Some(OsStr::new(""))).is_err());
}

#[test]
fn prepare_creates_nested_directories_idempotently_and_preserves_files() {
    let fixture = FixtureDir::new();
    let root = fixture.0.join("nested").join("models");
    ModelDirectory::prepare(root.clone()).unwrap();
    let sentinel = write(&root, "keep.txt", b"existing user data");
    let model = write(&root, "weights.gguf", &Gguf::valid().bytes);
    let original = fs::read(&model).unwrap();
    let directory = ModelDirectory::prepare(root.clone()).unwrap();
    let catalog = directory.scan().unwrap();
    assert_eq!(catalog.root, root);
    assert_eq!(fs::read(sentinel).unwrap(), b"existing user data");
    assert_eq!(fs::read(model).unwrap(), original);
}

#[test]
fn prepare_rejects_a_regular_file_as_the_directory() {
    let fixture = FixtureDir::new();
    let file = fixture.0.join("models");
    fs::write(&file, b"preserve me").unwrap();
    assert!(ModelDirectory::prepare(file.clone()).is_err());
    assert_eq!(fs::read(file).unwrap(), b"preserve me");
}

#[test]
fn discovery_reads_valid_weight_metadata_and_ignores_non_gguf_files() {
    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let valid = Gguf::valid();
    let path = write(&fixture.root(), "nested/model.gguf", &valid.bytes);
    write(&fixture.root(), "README.md", b"not a model");
    write(&fixture.root(), "download.gguf.part", b"partial download");
    let catalog = directory.scan().unwrap();
    assert!(catalog.rejected.is_empty());
    assert_eq!(catalog.models.len(), 1);
    let model = &catalog.models[0];
    assert_eq!(model.path, path);
    assert!(!model.id.is_empty());
    assert_eq!(model.size_bytes, valid.bytes.len() as u64);
    assert_eq!(model.gguf_version, 3);
    assert_eq!(model.tensor_count, 1);
    assert_eq!(model.architecture.as_deref(), Some("llama"));
    assert_eq!(model.file_type, Some(0));
}

#[test]
fn discovery_ids_are_unique_sorted_and_stable_across_scans() {
    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    for name in ["zeta.gguf", "b/model.gguf", "a/model.gguf", "alpha.gguf"] {
        write(&fixture.root(), name, &Gguf::valid().bytes);
    }
    let first = directory.scan().unwrap();
    let second = directory.scan().unwrap();
    let ids: Vec<_> = first.models.iter().map(|model| model.id.clone()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(ids.len(), 4);
    assert_eq!(ids, sorted);
    assert_eq!(
        ids,
        second
            .models
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn tokenizer_only_files_are_rejected_without_hiding_valid_models() {
    assert_rejected(&Gguf::new(false, None).bytes);
}

#[test]
fn malformed_magic_version_and_truncated_headers_are_rejected() {
    for length in [0, 3, 4, 7, 8, 15, 16, 23] {
        assert_rejected(&Gguf::valid().bytes[..length]);
    }
    let mut bad_magic = Gguf::valid();
    bad_magic.bytes[..4].copy_from_slice(b"NOPE");
    assert_rejected(&bad_magic.bytes);
    let mut bad_version = Gguf::valid();
    bad_version.u32(4, u32::MAX);
    assert_rejected(&bad_version.bytes);
}

#[test]
fn metadata_counts_lengths_and_unknown_types_are_bounded() {
    let mut excessive_count = Gguf::valid();
    excessive_count.u64(16, u64::MAX);
    assert_rejected(&excessive_count.bytes);
    let mut excessive_string = header(1, 1);
    excessive_string.extend_from_slice(&u64::MAX.to_le_bytes());
    assert_rejected(&excessive_string);
    let mut unknown_type = header(1, 1);
    string(&mut unknown_type, "unknown.metadata");
    unknown_type.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_rejected(&unknown_type);
    let mut excessive_array = header(1, 1);
    string(&mut excessive_array, "tokenizer.ggml.tokens");
    excessive_array.extend_from_slice(&9_u32.to_le_bytes()); // ARRAY
    excessive_array.extend_from_slice(&8_u32.to_le_bytes()); // STRING elements
    excessive_array.extend_from_slice(&u64::MAX.to_le_bytes());
    assert_rejected(&excessive_array);
}

#[test]
fn truncated_tensor_descriptors_and_extreme_dimensions_are_rejected() {
    let valid = Gguf::valid();
    for length in [0, 8, 14, 18, 26, 34, 38, 45] {
        assert_rejected(&valid.bytes[..valid.tensor_start + length]);
    }
    let mut tensor_count = Gguf::valid();
    tensor_count.u64(8, u64::MAX);
    assert_rejected(&tensor_count.bytes);
    let mut dimensions = Gguf::valid();
    dimensions.u32(dimensions.tensor_start + 14, u32::MAX);
    assert_rejected(&dimensions.bytes);
    let mut dimension_size = Gguf::valid();
    dimension_size.u64(dimension_size.tensor_start + 18, u64::MAX);
    assert_rejected(&dimension_size.bytes);
}

#[test]
fn truncated_tensor_payload_and_out_of_bounds_offsets_are_rejected() {
    let valid = Gguf::valid();
    assert_rejected(&valid.bytes[..valid.payload_start]);
    assert_rejected(&valid.bytes[..valid.payload_start + 3]);
    let mut invalid_offset = Gguf::valid();
    invalid_offset.u64(invalid_offset.tensor_start + 38, u64::MAX);
    assert_rejected(&invalid_offset.bytes);
}

#[test]
fn unknown_tensor_types_are_rejected_without_panicking_or_hiding_valid_models() {
    let mut invalid_type = Gguf::valid();
    invalid_type.u32(invalid_type.tensor_start + 34, u32::MAX);
    assert_rejected(&invalid_type.bytes);
}

#[test]
fn split_models_are_rejected_instead_of_advertising_an_incomplete_model() {
    assert_rejected(&Gguf::new(true, Some(2)).bytes);
}

#[test]
fn removed_candidates_disappear_and_missing_root_is_an_error() {
    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let path = write(&fixture.root(), "removed.gguf", &Gguf::valid().bytes);
    assert_eq!(directory.scan().unwrap().models.len(), 1);
    fs::remove_file(path).unwrap();
    assert!(directory.scan().unwrap().models.is_empty());
    fs::remove_dir(fixture.root()).unwrap();
    assert!(directory.scan().is_err());
}

#[cfg(unix)]
#[test]
fn symlink_duplicates_loops_and_missing_targets_are_not_followed() {
    use std::os::unix::fs::symlink;

    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let original = write(&fixture.root(), "original.gguf", &Gguf::valid().bytes);
    symlink(&original, fixture.root().join("duplicate.gguf")).unwrap();
    symlink(fixture.root(), fixture.root().join("loop")).unwrap();
    symlink(
        fixture.root().join("missing"),
        fixture.root().join("dangling.gguf"),
    )
    .unwrap();
    let catalog = directory.scan().unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].path, original);
}

#[cfg(unix)]
#[test]
fn unreadable_directory_is_an_error_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let root = fixture.root();
    let permissions = fs::metadata(&root).unwrap().permissions();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o0)).unwrap();
    let permissions_enforced = fs::read_dir(&root).is_err();
    let scanned = directory.scan();
    let prepared = ModelDirectory::prepare(root.clone());
    fs::set_permissions(&root, permissions).unwrap();
    // Root/CAP_DAC_OVERRIDE can read mode 000; that environment cannot
    // demonstrate denial, so do not assert an artificial permission failure.
    if !permissions_enforced {
        return;
    }
    assert!(scanned.is_err());
    assert!(prepared.is_err());
}

#[cfg(unix)]
#[test]
fn unreadable_candidate_has_a_diagnostic_and_other_models_remain_visible() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let locked = write(&fixture.root(), "locked.gguf", &Gguf::valid().bytes);
    let good = write(&fixture.root(), "good.gguf", &Gguf::valid().bytes);
    let permissions = fs::metadata(&locked).unwrap().permissions();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
    let permissions_enforced = fs::File::open(&locked).is_err();
    let result = directory.scan();
    fs::set_permissions(&locked, permissions).unwrap();
    if !permissions_enforced {
        return;
    }
    let catalog = result.unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].path, good);
    assert!(catalog
        .rejected
        .iter()
        .any(|entry| entry.path == locked && !entry.error.is_empty()));
}

#[test]
fn little_endian_gguf_v2_preserves_model_metadata() {
    let mut valid = Gguf::valid();
    valid.u32(4, 2);
    let catalog = assert_discovered(&valid.bytes);
    let model = &catalog.models[0];
    assert_eq!(model.gguf_version, 2);
    assert_eq!(model.tensor_count, 1);
    assert_eq!(model.architecture.as_deref(), Some("llama"));
    assert_eq!(model.file_type, Some(0));
}

#[test]
fn quantized_tensor_payload_spans_follow_upstream_block_sizes() {
    // Independent examples from ggml-common.h: a Q4_0 block stores a half
    // scale + 16 bytes; Q4_K stores two half scales + 12 + 128 bytes;
    // Q6_K stores a half scale + 128 + 64 + 16 bytes.
    for (kind, dimensions, payload_bytes) in
        [(2, [32, 2], 36), (12, [256, 2], 288), (14, [512, 2], 840)]
    {
        let bytes = tensor_file(
            &[],
            0,
            &[Tensor {
                name: "weight",
                dimensions: &dimensions,
                kind,
                offset: 0,
            }],
            payload_bytes,
        );
        assert_discovered(&bytes);
        assert_rejected(&bytes[..bytes.len() - 1]);
    }
}

#[test]
fn quantized_rows_must_be_divisible_even_when_total_elements_are_divisible() {
    for (kind, dimensions, payload_bytes) in [(2, [16, 2], 18), (12, [128, 2], 144)] {
        let bytes = tensor_file(
            &[],
            0,
            &[Tensor {
                name: "weight",
                dimensions: &dimensions,
                kind,
                offset: 0,
            }],
            payload_bytes,
        );
        assert_rejected(&bytes);
    }
}

#[test]
fn metadata_arrays_are_skipped_without_losing_following_architecture() {
    let mut metadata = Vec::new();
    string(&mut metadata, "tokenizer.ggml.tokens");
    metadata.extend_from_slice(&9_u32.to_le_bytes()); // ARRAY
    metadata.extend_from_slice(&8_u32.to_le_bytes()); // STRING
    metadata.extend_from_slice(&3_u64.to_le_bytes());
    for value in ["", "обычный токен", "last"] {
        string(&mut metadata, value);
    }
    string(&mut metadata, "tokenizer.ggml.scores");
    metadata.extend_from_slice(&9_u32.to_le_bytes());
    metadata.extend_from_slice(&6_u32.to_le_bytes()); // FLOAT32
    metadata.extend_from_slice(&3_u64.to_le_bytes());
    for value in [0_f32, -0.5_f32, 1_f32] {
        metadata.extend_from_slice(&value.to_le_bytes());
    }
    string(&mut metadata, "custom.flags");
    metadata.extend_from_slice(&9_u32.to_le_bytes());
    metadata.extend_from_slice(&7_u32.to_le_bytes()); // BOOL
    metadata.extend_from_slice(&2_u64.to_le_bytes());
    metadata.extend_from_slice(&[0, 1]);
    string(&mut metadata, "general.architecture");
    metadata.extend_from_slice(&8_u32.to_le_bytes());
    string(&mut metadata, "llama");
    let bytes = tensor_file(
        &metadata,
        4,
        &[Tensor {
            name: "weight",
            dimensions: &[1, 1],
            kind: 0,
            offset: 0,
        }],
        32,
    );
    let catalog = assert_discovered(&bytes);
    assert_eq!(catalog.models[0].architecture.as_deref(), Some("llama"));
}

#[test]
fn invalid_alignment_is_rejected_before_tensor_extent_arithmetic() {
    for alignment in [0_u32, 3, 1 << 21, u32::MAX] {
        let mut metadata = Vec::new();
        string(&mut metadata, "general.alignment");
        metadata.extend_from_slice(&4_u32.to_le_bytes());
        metadata.extend_from_slice(&alignment.to_le_bytes());
        let bytes = tensor_file(
            &metadata,
            1,
            &[Tensor {
                name: "weight",
                dimensions: &[1, 1],
                kind: 0,
                offset: 0,
            }],
            32,
        );
        assert_rejected(&bytes);
    }
}

#[test]
fn tensor_offsets_require_distinct_contiguous_aligned_spans() {
    for second_offset in [0, 4, 33, 64] {
        let bytes = tensor_file(
            &[],
            0,
            &[
                Tensor {
                    name: "first",
                    dimensions: &[1, 1],
                    kind: 0,
                    offset: 0,
                },
                Tensor {
                    name: "second",
                    dimensions: &[1, 1],
                    kind: 0,
                    offset: second_offset,
                },
            ],
            96,
        );
        assert_rejected(&bytes);
    }
    let valid = tensor_file(
        &[],
        0,
        &[
            Tensor {
                name: "first",
                dimensions: &[1, 1],
                kind: 0,
                offset: 0,
            },
            Tensor {
                name: "second",
                dimensions: &[1, 1],
                kind: 0,
                offset: 32,
            },
        ],
        64,
    );
    let catalog = assert_discovered(&valid);
    assert_eq!(catalog.models[0].tensor_count, 2);
}

#[cfg(unix)]
#[test]
fn non_utf8_candidate_diagnostics_do_not_prevent_catalog_json_serialization() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = FixtureDir::new();
    let directory = fixture.directory();
    let path = fixture
        .root()
        .join(OsString::from_vec(b"invalid-\xff.gguf".to_vec()));
    fs::write(&path, Gguf::valid().bytes).unwrap();
    write(&fixture.root(), "good.gguf", &Gguf::valid().bytes);
    let catalog = directory.scan().unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert!(catalog.rejected.iter().any(|entry| entry.path == path));
    assert!(serde_json::to_value(&catalog).is_ok());
}
