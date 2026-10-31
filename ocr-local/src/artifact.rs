use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Error, Limits};

/// The purpose of an explicitly supplied file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRole {
    /// The ONNX Runtime shared library.
    Runtime,
    /// The PP-OCR text detection ONNX model.
    Detector,
    /// The matching text recognition ONNX model.
    Recognizer,
    /// The recognition model's UTF-8 character dictionary.
    Dictionary,
}

/// A caller-supplied local path and the exact SHA-256 its bytes must match.
#[derive(Clone)]
pub struct FileSpec {
    path: PathBuf,
    digest: [u8; 32],
}

impl FileSpec {
    /// Name a local file without reading it.
    pub fn new(path: impl Into<PathBuf>, sha256: [u8; 32]) -> Self {
        Self { path: path.into(), digest: sha256 }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

impl fmt::Debug for FileSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileSpec(<redacted>)")
    }
}

/// Matching single-file ONNX models and their character dictionary.
#[derive(Debug, Clone)]
pub struct ModelFiles {
    /// Detection model file and its expected SHA-256.
    pub detector: FileSpec,
    /// Recognition model file and its expected SHA-256.
    pub recognizer: FileSpec,
    /// Dictionary file and its expected SHA-256.
    pub dictionary: FileSpec,
}

#[derive(PartialEq, Eq)]
pub(crate) struct VerifiedModels {
    pub(crate) detector: Vec<u8>,
    pub(crate) recognizer: Vec<u8>,
    pub(crate) dictionary: Vec<String>,
}

impl fmt::Debug for VerifiedModels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerifiedModels(<redacted>)")
    }
}

impl ModelFiles {
    pub(crate) fn read(&self, limits: &Limits) -> Result<VerifiedModels, Error> {
        let detector =
            read_verified(&self.detector, FileRole::Detector, limits.max_artifact_bytes)?;
        let recognizer =
            read_verified(&self.recognizer, FileRole::Recognizer, limits.max_artifact_bytes)?;
        let bytes =
            read_verified(&self.dictionary, FileRole::Dictionary, limits.max_dictionary_bytes)?;
        let dictionary = dictionary(bytes)?;
        Ok(VerifiedModels { detector, recognizer, dictionary })
    }
}

fn dictionary(bytes: Vec<u8>) -> Result<Vec<String>, Error> {
    let text = String::from_utf8(bytes).map_err(|_| Error::Dictionary)?;
    let entries: Vec<String> = text
        .lines()
        .map(|line| {
            let mut chars = line.chars();
            if chars.next().is_none_or(|c| c == '\0') || chars.next().is_some() {
                return Err(Error::Dictionary);
            }
            Ok(line.to_owned())
        })
        .collect::<Result<_, _>>()?;
    if entries.is_empty() {
        return Err(Error::Dictionary);
    }
    Ok(entries)
}

pub(crate) fn read_verified(spec: &FileSpec, role: FileRole, cap: usize) -> Result<Vec<u8>, Error> {
    let metadata = spec.path.metadata().map_err(|_| Error::FileUnreadable(role))?;
    if !metadata.is_file() {
        return Err(Error::FileUnreadable(role));
    }
    if metadata.len() > cap as u64 {
        return Err(Error::FileTooLarge(role));
    }
    let file = File::open(&spec.path).map_err(|_| Error::FileUnreadable(role))?;
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1).read_to_end(&mut bytes).map_err(|_| Error::FileUnreadable(role))?;
    if bytes.len() > cap {
        return Err(Error::FileTooLarge(role));
    }
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != spec.digest {
        return Err(Error::DigestMismatch(role));
    }
    Ok(bytes)
}
