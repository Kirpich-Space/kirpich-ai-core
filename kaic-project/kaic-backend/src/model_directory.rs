//! Local GGUF inventory. File discovery grants neither capabilities nor a memory budget.
//!
//! Format/layout evidence: upstream ggml/include/gguf.h, ggml/src/gguf.cpp,
//! ggml/include/ggml.h and ggml/src/ggml-common.h. Only little-endian v2/v3
//! single-file GGUF is supported. We inspect metadata and tensor extents, never weights.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Serialize, Serializer};

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 32;
const MAX_KV: u64 = 100_000;
const MAX_TENSORS: u64 = 100_000;
const MAX_ARRAY_ELEMENTS: u64 = 16_000_000;
const MAX_METADATA_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STRING_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RETAINED_NAMES: u64 = 32 * 1024 * 1024;
const MAX_ALIGNMENT: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ModelDirectory {
    root: PathBuf,
}

#[derive(Serialize, Debug, Clone)]
pub struct LocalCatalog {
    #[serde(serialize_with = "serialize_display_path")]
    pub root: PathBuf,
    pub models: Vec<LocalModel>,
    pub rejected: Vec<RejectedModel>,
}

#[derive(Serialize, Debug, Clone)]
pub struct LocalModel {
    /// UTF-8 filename path relative to the inventory root, with '/' separators.
    pub id: String,
    #[serde(serialize_with = "serialize_display_path")]
    pub path: PathBuf,
    pub size_bytes: u64,
    pub gguf_version: u32,
    pub tensor_count: u64,
    pub architecture: Option<String>,
    pub file_type: Option<u32>,
}

#[derive(Serialize, Debug, Clone)]
pub struct RejectedModel {
    #[serde(serialize_with = "serialize_display_path")]
    pub path: PathBuf,
    pub error: String,
}

/// Paths are display data in the catalogue, never filesystem identifiers to
/// round-trip. Preserve the exact OS path internally even when JSON needs a
/// replacement character for a non-UTF-8 filename or parent directory.
fn serialize_display_path<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

impl ModelDirectory {
    pub fn from_env() -> Result<Self> {
        Self::prepare(Self::resolve(
            std::env::var_os("KAIC_MODELS_DIR").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )?)
    }

    /// Explicit overrides are preserved, including relative paths. Defaults
    /// never consult uppercase `Models` and never expand shell syntax.
    pub fn resolve(override_dir: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
        if let Some(path) = override_dir {
            ensure!(!path.is_empty(), "KAIC_MODELS_DIR must not be empty");
            return Ok(PathBuf::from(path));
        }
        let home = home.filter(|value| !value.is_empty()).context(
            "HOME is unavailable; set KAIC_MODELS_DIR explicitly for the local model directory",
        )?;
        Ok(PathBuf::from(home).join("models"))
    }

    pub fn prepare(root: PathBuf) -> Result<Self> {
        ensure!(
            !root.as_os_str().is_empty(),
            "model directory must not be empty"
        );
        fs::create_dir_all(&root)
            .with_context(|| format!("cannot create model directory {}", root.display()))?;
        let root = fs::canonicalize(&root)
            .with_context(|| format!("cannot resolve model directory {}", root.display()))?;
        fs::read_dir(&root)
            .with_context(|| format!("cannot read model directory {}", root.display()))?;
        Ok(Self { root })
    }

    /// Call once during startup and retain the snapshot. No background polling,
    /// model loading, downloads, registry writes or runtime memory estimates.
    pub fn scan(&self) -> Result<LocalCatalog> {
        let mut catalog = LocalCatalog {
            root: self.root.clone(),
            models: Vec::new(),
            rejected: Vec::new(),
        };
        let mut entries_left = MAX_ENTRIES;
        self.scan_dir(&self.root, 0, &mut entries_left, &mut catalog)?;
        catalog.models.sort_by(|a, b| a.id.cmp(&b.id));
        catalog.rejected.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(catalog)
    }

    fn scan_dir(
        &self,
        dir: &Path,
        depth: usize,
        entries_left: &mut usize,
        catalog: &mut LocalCatalog,
    ) -> Result<()> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
            ensure!(
                *entries_left > 0,
                "model directory entry limit exceeded ({MAX_ENTRIES})"
            );
            *entries_left -= 1;
            entries.push(entry.with_context(|| format!("cannot enumerate {}", dir.display()))?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let result = (|| -> Result<()> {
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    if is_gguf(&path) {
                        bail!("symlink GGUF files are unsupported; use a regular file");
                    }
                    return Ok(());
                }
                if kind.is_dir() {
                    ensure!(
                        depth < MAX_DEPTH,
                        "model directory depth limit exceeded ({MAX_DEPTH})"
                    );
                    self.scan_dir(&path, depth + 1, entries_left, catalog)?;
                } else if is_gguf(&path) {
                    ensure!(kind.is_file(), "GGUF path is not a regular file");
                    catalog.models.push(self.inspect(&path)?);
                }
                Ok(())
            })();
            if let Err(error) = result {
                catalog.rejected.push(RejectedModel {
                    path,
                    error: format!("{error:#}"),
                });
                // An exhausted global budget must stop the whole traversal.
                ensure!(
                    *entries_left > 0,
                    "model directory entry limit exceeded ({MAX_ENTRIES})"
                );
            }
        }
        Ok(())
    }

    fn inspect(&self, path: &Path) -> Result<LocalModel> {
        ensure!(
            !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "symlink GGUF file"
        );
        let canonical = fs::canonicalize(path)?;
        ensure!(
            canonical.starts_with(&self.root),
            "GGUF path escapes the model directory"
        );
        let relative = path.strip_prefix(&self.root)?;
        let id = relative
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .context("GGUF filename is not UTF-8")
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        let file = File::open(&canonical)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "GGUF path is not a regular file");
        let size_bytes = metadata.len();
        let parsed = parse_gguf(file, size_bytes)?;
        Ok(LocalModel {
            id,
            path: canonical,
            size_bytes,
            gguf_version: parsed.version,
            tensor_count: parsed.tensor_count,
            architecture: parsed.architecture,
            file_type: parsed.file_type,
        })
    }
}

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|value| value.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}

struct ParsedGguf {
    version: u32,
    tensor_count: u64,
    architecture: Option<String>,
    file_type: Option<u32>,
}

/// Position and lengths are checked before every read/seek; skipped strings and
/// arrays do not allocate, and no seek/read ever visits the weight section.
struct GgufReader {
    input: BufReader<File>,
    length: u64,
    position: u64,
    array_elements_left: u64,
    retained_names_left: u64,
}

impl GgufReader {
    fn new(file: File, length: u64) -> Self {
        Self {
            input: BufReader::new(file),
            length,
            position: 0,
            array_elements_left: MAX_ARRAY_ELEMENTS,
            retained_names_left: MAX_RETAINED_NAMES,
        }
    }

    fn end(&self, bytes: u64) -> Result<u64> {
        let end = self
            .position
            .checked_add(bytes)
            .context("GGUF offset overflow")?;
        ensure!(
            end <= self.length,
            "truncated GGUF metadata or tensor descriptor"
        );
        ensure!(
            end <= MAX_METADATA_BYTES,
            "GGUF metadata exceeds inspection limit"
        );
        Ok(end)
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.end(N as u64)?;
        let mut bytes = [0; N];
        self.input
            .read_exact(&mut bytes)
            .context("truncated GGUF")?;
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }

    fn skip(&mut self, bytes: u64) -> Result<()> {
        let end = self.end(bytes)?;
        self.input.seek(SeekFrom::Start(end))?;
        self.position = end;
        Ok(())
    }

    fn string(&mut self, limit: u64) -> Result<String> {
        let length = self.u64()?;
        ensure!(length <= limit, "GGUF string exceeds inspection limit");
        let end = self.end(length)?;
        self.retained_names_left = self
            .retained_names_left
            .checked_sub(length)
            .context("GGUF retained names exceed inspection limit")?;
        let length = usize::try_from(length).context("GGUF string is too large")?;
        let mut bytes = vec![0; length];
        self.input
            .read_exact(&mut bytes)
            .context("truncated GGUF string")?;
        self.position = end;
        let string = String::from_utf8(bytes).context("GGUF string is not UTF-8")?;
        ensure!(!string.contains('\0'), "GGUF name contains NUL");
        Ok(string)
    }

    fn skip_string(&mut self) -> Result<()> {
        let length = self.u64()?;
        ensure!(
            length <= MAX_STRING_BYTES,
            "GGUF string exceeds inspection limit"
        );
        self.skip(length)
    }

    fn skip_value(&mut self, kind: u32) -> Result<()> {
        match kind {
            8 => self.skip_string(),
            9 => {
                let element_kind = self.u32()?;
                ensure!(element_kind != 9, "nested GGUF arrays are unsupported");
                let count = self.u64()?;
                self.array_elements_left = self
                    .array_elements_left
                    .checked_sub(count)
                    .context("GGUF arrays exceed inspection limit")?;
                if element_kind == 8 {
                    for _ in 0..count {
                        self.skip_string()?;
                    }
                    Ok(())
                } else if element_kind == 7 {
                    for _ in 0..count {
                        ensure!(self.u8()? <= 1, "invalid GGUF boolean");
                    }
                    Ok(())
                } else {
                    let size = scalar_size(element_kind)?;
                    self.skip(
                        count
                            .checked_mul(size)
                            .context("GGUF array size overflow")?,
                    )
                }
            }
            7 => {
                ensure!(self.u8()? <= 1, "invalid GGUF boolean");
                Ok(())
            }
            _ => self.skip(scalar_size(kind)?),
        }
    }

    fn unsigned(&mut self, kind: u32) -> Result<u64> {
        match kind {
            0 => Ok(u64::from(self.u8()?)),
            2 => Ok(u64::from(self.u16()?)),
            4 => Ok(u64::from(self.u32()?)),
            10 => self.u64(),
            _ => bail!("GGUF split.count must be an unsigned integer"),
        }
    }
}

fn scalar_size(kind: u32) -> Result<u64> {
    match kind {
        0 | 1 | 7 => Ok(1),
        2 | 3 => Ok(2),
        4..=6 => Ok(4),
        10..=12 => Ok(8),
        _ => bail!("unsupported GGUF metadata type {kind}"),
    }
}

fn parse_gguf(file: File, length: u64) -> Result<ParsedGguf> {
    let mut reader = GgufReader::new(file, length);
    ensure!(&reader.bytes::<4>()? == b"GGUF", "invalid GGUF magic");
    let version = reader.u32()?;
    ensure!(
        version == 2 || version == 3,
        "unsupported GGUF version {version}; expected little-endian v2/v3"
    );
    let tensor_count = reader.u64()?;
    ensure!(tensor_count > 0, "GGUF contains zero tensors");
    ensure!(
        tensor_count <= MAX_TENSORS,
        "GGUF tensor count exceeds inspection limit"
    );
    let kv_count = reader.u64()?;
    ensure!(
        kv_count <= MAX_KV,
        "GGUF metadata count exceeds inspection limit"
    );
    let mut architecture = None;
    let mut file_type = None;
    let mut alignment = 32;
    let mut keys = HashSet::new();
    for _ in 0..kv_count {
        let key = reader.string(16 * 1024)?;
        ensure!(!key.is_empty(), "empty GGUF metadata key");
        let kind = reader.u32()?;
        match key.as_str() {
            "general.architecture" => {
                ensure!(kind == 8, "GGUF general.architecture must be a string");
                let value = reader.string(1024)?;
                ensure!(!value.is_empty(), "GGUF general.architecture is empty");
                architecture = Some(value);
            }
            "general.file_type" => {
                ensure!(kind == 4, "GGUF general.file_type must be uint32");
                file_type = Some(reader.u32()?);
            }
            "general.alignment" => {
                ensure!(kind == 4, "GGUF general.alignment must be uint32");
                alignment = u64::from(reader.u32()?);
                ensure!(
                    alignment.is_power_of_two() && alignment <= MAX_ALIGNMENT,
                    "GGUF alignment must be a power of two within the inspection limit"
                );
            }
            "split.count" => {
                let count = reader.unsigned(kind)?;
                ensure!(count > 0, "GGUF split.count must be positive");
                ensure!(
                    count == 1,
                    "split GGUF is unsupported (split.count={count}); use a single-file model"
                );
            }
            _ => reader.skip_value(kind)?,
        }
        ensure!(keys.insert(key), "duplicate GGUF metadata key");
    }
    drop(keys);

    let mut tensor_names = HashSet::new();
    let mut padded_end = 0u64;
    let mut actual_end = 0u64;
    for _ in 0..tensor_count {
        // GGML_MAX_NAME is 64, including the terminating NUL in memory.
        let name = reader.string(63)?;
        ensure!(!name.is_empty(), "empty GGUF tensor name");
        ensure!(tensor_names.insert(name), "duplicate GGUF tensor name");
        let dims = reader.u32()?;
        ensure!(
            dims <= 4,
            "GGUF tensor dimension count exceeds GGML_MAX_DIMS"
        );
        let mut elements = 1u64;
        let mut row_elements = 1u64;
        for dimension in 0..dims {
            let extent = reader.u64()?;
            ensure!(
                extent > 0 && extent <= i64::MAX as u64,
                "invalid GGUF tensor dimension"
            );
            if dimension == 0 {
                row_elements = extent;
            }
            elements = elements
                .checked_mul(extent)
                .context("GGUF tensor shape overflow")?;
            ensure!(
                elements <= i64::MAX as u64,
                "GGUF tensor element count overflow"
            );
        }
        let kind = reader.u32()?;
        let (block_elements, block_bytes) = tensor_layout(kind)?;
        ensure!(
            row_elements % block_elements == 0,
            "GGUF tensor row is not divisible by GGML block size"
        );
        let bytes = (elements / block_elements)
            .checked_mul(block_bytes)
            .context("GGUF tensor byte size overflow")?;
        let offset = reader.u64()?;
        // Upstream gguf.cpp requires tensors to follow descriptor order with
        // aligned contiguous spans. This also rejects overlaps and offset wrap.
        ensure!(
            offset == padded_end,
            "invalid GGUF tensor offset {offset}; expected {padded_end}"
        );
        actual_end = offset
            .checked_add(bytes)
            .context("GGUF tensor payload offset overflow")?;
        padded_end = align_up(actual_end, alignment)?;
    }
    let data_start = align_up(reader.position, alignment)?;
    let required_end = data_start
        .checked_add(actual_end)
        .context("GGUF payload extent overflow")?;
    ensure!(
        required_end <= length,
        "truncated GGUF tensor payload: need {required_end} bytes, have {length}"
    );
    // Detect truncation concurrent with inspection without touching the weights.
    ensure!(
        reader.input.get_ref().metadata()?.len() == length,
        "GGUF file changed during inspection"
    );
    Ok(ParsedGguf {
        version,
        tensor_count,
        architecture,
        file_type,
    })
}

fn align_up(value: u64, alignment: u64) -> Result<u64> {
    Ok(value
        .checked_add(alignment - 1)
        .context("GGUF alignment overflow")?
        & !(alignment - 1))
}

/// (elements per block, bytes per block), from upstream ggml-common.h static
/// assertions and ggml.c type_traits. Removed/repacked/unknown types fail closed.
fn tensor_layout(kind: u32) -> Result<(u64, u64)> {
    let layout = match kind {
        0 => (1, 4),           // F32
        1 | 25 | 30 => (1, 2), // F16, I16, BF16
        2 | 20 => (32, 18),    // Q4_0, IQ4_NL
        3 => (32, 20),         // Q4_1
        6 => (32, 22),         // Q5_0
        7 => (32, 24),         // Q5_1
        8 => (32, 34),         // Q8_0
        9 => (32, 36),         // Q8_1
        10 => (256, 84),       // Q2_K
        11 => (256, 110),      // Q3_K
        12 => (256, 144),      // Q4_K
        13 => (256, 176),      // Q5_K
        14 => (256, 210),      // Q6_K
        15 => (256, 292),      // Q8_K
        16 => (256, 66),       // IQ2_XXS
        17 => (256, 74),       // IQ2_XS
        18 => (256, 98),       // IQ3_XXS
        19 => (256, 50),       // IQ1_S
        21 => (256, 110),      // IQ3_S
        22 => (256, 82),       // IQ2_S
        23 => (256, 136),      // IQ4_XS
        24 => (1, 1),          // I8
        26 => (1, 4),          // I32
        27 | 28 => (1, 8),     // I64, F64
        29 => (256, 56),       // IQ1_M
        34 => (256, 54),       // TQ1_0
        35 => (256, 66),       // TQ2_0
        39 => (32, 17),        // MXFP4
        40 => (64, 36),        // NVFP4
        41 => (128, 18),       // Q1_0
        42 => (64, 18),        // Q2_0
        _ => bail!("unsupported GGML tensor type {kind}; payload size cannot be validated"),
    };
    Ok(layout)
}

#[cfg(test)]
#[path = "model_directory_tests.rs"]
mod tests;
