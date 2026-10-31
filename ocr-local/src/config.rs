use crate::Error;

/// Admission bounds; decoder allocation accounting is best effort, not a process memory limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Maximum bytes in an encoded PNG or JPEG.
    pub max_image_bytes: usize,
    /// Maximum width or height of an input or cropped image.
    pub max_dimension: u32,
    /// Maximum pixels in an input or cropped image.
    pub max_pixels: u64,
    /// Maximum decoded source plus normalized RGBA and RGB output buffer bytes.
    pub max_decode_bytes: u64,
    /// Maximum bytes in each supplied runtime or model file.
    pub max_artifact_bytes: usize,
    /// Maximum bytes in the UTF-8 character dictionary.
    pub max_dictionary_bytes: usize,
    /// Maximum detected text regions returned for one image.
    pub max_lines: usize,
    /// Maximum total UTF-8 bytes of recognized text.
    pub max_text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_image_bytes: 32 << 20,
            max_dimension: 8192,
            max_pixels: 16 << 20,
            max_decode_bytes: 128 << 20,
            max_artifact_bytes: 1 << 30,
            max_dictionary_bytes: 4 << 20,
            max_lines: 4096,
            max_text_bytes: 1 << 20,
        }
    }
}

impl Limits {
    /// Check all admission bounds without opening files or loading the runtime.
    ///
    /// # Errors
    /// Returns [`Error::Config`] naming the first invalid bound.
    pub fn validate(&self) -> Result<(), Error> {
        for (name, value) in [
            ("max_image_bytes", self.max_image_bytes),
            ("max_artifact_bytes", self.max_artifact_bytes),
            ("max_dictionary_bytes", self.max_dictionary_bytes),
            ("max_text_bytes", self.max_text_bytes),
        ] {
            if value == 0 || value == usize::MAX {
                return Err(Error::Config(name));
            }
        }
        if self.max_dimension == 0 {
            return Err(Error::Config("max_dimension"));
        }
        if self.max_pixels == 0 {
            return Err(Error::Config("max_pixels"));
        }
        if crate::decode::buffer_bytes(1, 1).is_none_or(|minimum| self.max_decode_bytes < minimum) {
            return Err(Error::Config("max_decode_bytes"));
        }
        if !(1..=100_000).contains(&self.max_lines) {
            return Err(Error::Config("max_lines"));
        }
        Ok(())
    }
}

/// Configuration for two sequential CPU sessions and a private Rayon pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Private Rayon workers; every ONNX session separately uses one intra/inter-op thread.
    pub threads: usize,
    /// Input and output admission bounds.
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self { threads: 2, limits: Limits::default() }
    }
}

impl Config {
    /// Check the worker count and every admission bound without side effects.
    ///
    /// # Errors
    /// Returns [`Error::Config`] naming the first invalid setting.
    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=8).contains(&self.threads) {
            return Err(Error::Config("threads"));
        }
        self.limits.validate()
    }
}
