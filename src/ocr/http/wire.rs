use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::ocr::OcrImage;

/// Top-level fields of a request body that the engine sets, or whose value would change what it
/// reads back.
pub(super) const RESERVED: [&str; 6] =
    ["messages", "model", "max_tokens", "temperature", "n", "stream"];

/// What every request body of one engine holds besides its image.
#[derive(Clone)]
pub(super) struct Template {
    pub(super) fields: Map<String, Value>,
    pub(super) prompt: String,
    pub(super) image_first: bool,
}

impl Template {
    /// The `chat/completions` body asking for the text of `image`.
    pub(super) fn body(&self, image: &OcrImage) -> Vec<u8> {
        let url = format!("data:{};base64,{}", image.media_type, STANDARD.encode(&image.bytes));
        let image = json!({ "type": "image_url", "image_url": { "url": url } });
        let text = json!({ "type": "text", "text": self.prompt });
        let content = if self.image_first { [image, text] } else { [text, image] };
        let mut body = self.fields.clone();
        body.insert("messages".into(), json!([{ "role": "user", "content": content }]));
        Value::Object(body).to_string().into_bytes()
    }
}

/// The Markdown a reply body carries: the first choice's string content when it finished
/// with `stop`, a leading `<think>` block and the whitespace after it dropped; `None` for any
/// other reply, an unclosed `<think>` block included.
pub(super) fn content(body: &[u8]) -> Option<String> {
    let reply: Value = serde_json::from_slice(body).ok()?;
    let choice = reply.get("choices")?.get(0)?;
    if choice.get("finish_reason")?.as_str()? != "stop" {
        return None;
    }
    let text = choice.get("message")?.get("content")?.as_str()?;
    let Some(thought) = text.trim_start().strip_prefix("<think>") else {
        return Some(text.to_owned());
    };
    let (_, after) = thought.split_once("</think>")?;
    Some(after.trim_start().to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;

    fn reply(content: &Value, finish: &str) -> Vec<u8> {
        let reply =
            json!({ "choices": [{ "message": { "content": content }, "finish_reason": finish }] });
        reply.to_string().into_bytes()
    }

    #[test]
    fn a_stopped_reply_gives_its_content_after_a_leading_think_block() {
        for (text, read) in [
            ("# A", Some("# A")),
            ("<think>x</think>\n# A", Some("# A")),
            ("  <think>x\n</think>  # A</think>", Some("# A</think>")),
            ("# A <think>x</think>", Some("# A <think>x</think>")),
            ("<think>never closed", None),
        ] {
            assert_eq!(content(&reply(&json!(text), "stop")).as_deref(), read, "{text}");
        }
        assert_eq!(content(&reply(&json!("# A"), "length")), None);
        assert_eq!(content(&reply(&Value::Null, "stop")), None);
        assert_eq!(content(&reply(&json!(["# A"]), "stop")), None);
        for body in [&b"{}"[..], b"{\"choices\": []}", b"not json", b""] {
            assert_eq!(content(body), None);
        }
    }

    #[test]
    fn a_body_holds_its_fields_and_one_data_uri_in_the_chosen_order() {
        let image =
            OcrImage { bytes: Arc::from(&b"\x89PNG"[..]), media_type: "image/png", dpi: None };
        let mut fields = Map::new();
        fields.insert("model".into(), json!("m"));
        fields.insert("top_p".into(), json!(0.5));
        let mut template = Template { fields, prompt: "Read it.".into(), image_first: true };
        let body: Value = serde_json::from_slice(&template.body(&image)).unwrap();
        let parts = &body["messages"][0]["content"];
        assert_eq!(
            (&body["model"], &body["top_p"], &body["messages"][0]["role"]),
            (&json!("m"), &json!(0.5), &json!("user"))
        );
        assert_eq!(parts[0]["image_url"]["url"], json!("data:image/png;base64,iVBORw=="));
        assert_eq!(parts[1], json!({ "type": "text", "text": "Read it." }));
        template.image_first = false;
        let body: Value = serde_json::from_slice(&template.body(&image)).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], json!("text"));
    }

    proptest! {
        #[test]
        fn any_body_reads_without_panicking(body in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _: Option<String> = content(&body);
        }

        #[test]
        fn any_stopped_text_keeps_what_follows_its_think_block(text in ".{0,64}") {
            let read = content(&reply(&json!(format!("<think>{text}</think>{text}")), "stop"));
            if !text.contains("</think>") {
                prop_assert_eq!(read, Some(text.trim_start().to_owned()));
            }
        }
    }
}
