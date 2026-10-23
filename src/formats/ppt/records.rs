//! The [MS-PPT] `RecordType` values this reader matches, and the instances
//! that tell the three `SlideListWithText` lists and the `HeadersFooters`
//! containers apart.

pub(super) const DOCUMENT: u16 = 0x03E8;
pub(super) const SLIDE: u16 = 0x03EE;
pub(super) const SLIDE_ATOM: u16 = 0x03EF;
pub(super) const NOTES: u16 = 0x03F0;
pub(super) const NOTES_ATOM: u16 = 0x03F1;
pub(super) const ENVIRONMENT: u16 = 0x03F2;
pub(super) const SLIDE_PERSIST_ATOM: u16 = 0x03F3;
pub(super) const MAIN_MASTER: u16 = 0x03F8;
pub(super) const EXTERNAL_OBJECT_LIST: u16 = 0x0409;
pub(super) const DRAWING_GROUP: u16 = 0x040B;
pub(super) const DRAWING: u16 = 0x040C;
pub(super) const FONT_COLLECTION: u16 = 0x07D5;
pub(super) const EXTERNAL_OBJECT_REF_ATOM: u16 = 0x0BC1;
pub(super) const PLACEHOLDER_ATOM: u16 = 0x0BC3;
pub(super) const OUTLINE_TEXT_REF_ATOM: u16 = 0x0F9E;
pub(super) const TEXT_HEADER_ATOM: u16 = 0x0F9F;
pub(super) const TEXT_CHARS_ATOM: u16 = 0x0FA0;
pub(super) const STYLE_TEXT_PROP_ATOM: u16 = 0x0FA1;
pub(super) const TEXT_MASTER_STYLE_ATOM: u16 = 0x0FA3;
pub(super) const TEXT_BYTES_ATOM: u16 = 0x0FA8;
pub(super) const FONT_ENTITY_ATOM: u16 = 0x0FB7;
pub(super) const CSTRING: u16 = 0x0FBA;
pub(super) const EXTERNAL_OLE_OBJECT_ATOM: u16 = 0x0FC3;
pub(super) const HANDOUT: u16 = 0x0FC9;
pub(super) const EXTERNAL_OLE_EMBED: u16 = 0x0FCC;
pub(super) const EXTERNAL_OLE_LINK: u16 = 0x0FCE;
pub(super) const EXTERNAL_HYPERLINK_ATOM: u16 = 0x0FD3;
pub(super) const EXTERNAL_HYPERLINK: u16 = 0x0FD7;
pub(super) const HEADERS_FOOTERS: u16 = 0x0FD9;
pub(super) const HEADERS_FOOTERS_ATOM: u16 = 0x0FDA;
pub(super) const TEXT_INTERACTIVE_INFO_ATOM: u16 = 0x0FDF;
pub(super) const EXTERNAL_OLE_CONTROL: u16 = 0x0FEE;
pub(super) const SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
pub(super) const INTERACTIVE_INFO_ATOM: u16 = 0x0FF3;
pub(super) const USER_EDIT_ATOM: u16 = 0x0FF5;
pub(super) const EXTERNAL_MEDIA_ATOM: u16 = 0x1004;
pub(super) const EXTERNAL_VIDEO: u16 = 0x1005;
pub(super) const EXTERNAL_AVI_MOVIE: u16 = 0x1006;
pub(super) const EXTERNAL_MCI_MOVIE: u16 = 0x1007;
pub(super) const EXTERNAL_MIDI_AUDIO: u16 = 0x100D;
pub(super) const EXTERNAL_WAV_AUDIO_LINK: u16 = 0x1010;
pub(super) const PROG_BINARY_TAG: u16 = 0x138A;
pub(super) const BINARY_TAG_DATA_BLOB: u16 = 0x138B;
pub(super) const PERSIST_DIRECTORY_ATOM: u16 = 0x1772;
pub(super) const COMMENT10: u16 = 0x2EE0;
pub(super) const CRYPT_SESSION10_CONTAINER: u16 = 0x2F14;

pub(super) const SLIDE_LIST_INSTANCE: u16 = 0;
pub(super) const MASTER_LIST_INSTANCE: u16 = 1;
pub(super) const NOTES_LIST_INSTANCE: u16 = 2;

pub(super) const PAGE_HEADERS_FOOTERS_INSTANCE: u16 = 0;
pub(super) const SLIDE_HEADERS_FOOTERS_INSTANCE: u16 = 3;
pub(super) const NOTES_HEADERS_FOOTERS_INSTANCE: u16 = 4;
