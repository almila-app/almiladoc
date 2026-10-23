/// Index into `Document::assets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct AssetId(pub usize);

/// An embedded binary asset serialized with `byte_len` in place of its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Asset {
    /// This asset's own index, so a detached `Asset` still identifies itself.
    pub id: AssetId,
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Package part or stream the asset came from, for provenance.
    pub origin_part: String,
    /// The payload, exactly as stored in the source.
    pub bytes: Vec<u8>,
}

#[cfg(feature = "serde")]
impl serde::Serialize for Asset {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut asset = serializer.serialize_struct("Asset", 4)?;
        asset.serialize_field("id", &self.id)?;
        asset.serialize_field("media_type", &self.media_type)?;
        asset.serialize_field("origin_part", &self.origin_part)?;
        asset.serialize_field("byte_len", &self.bytes.len())?;
        asset.end()
    }
}
