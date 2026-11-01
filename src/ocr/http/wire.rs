use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::TokenUsage;
use crate::ocr::{OcrFailureKind, OcrImage, RetryAfter};

/// Top-level fields of a request body that the engine sets, or whose value would change what it
/// reads back.
pub(super) const RESERVED: [&str; 8] = [
    "messages",
    "model",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "n",
    "stream",
    "service_tier",
];

/// How a request's messages carry the prompt and the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MessageLayout {
    /// One user message holding the image, then the prompt.
    #[default]
    ImageThenText,
    /// One user message holding the prompt, then the image.
    TextThenImage,
    /// A developer message holding the prompt, then a user message holding the image.
    DeveloperThenImage,
    /// A system message holding the prompt, then a user message holding the image.
    SystemThenImage,
}

/// The body field that carries the most tokens a reply holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TokenLimitField {
    /// `max_tokens`.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`.
    MaxCompletionTokens,
}

impl TokenLimitField {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// The resolution a model reads an image at, sent as `image_url.detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ImageDetail {
    /// `low`.
    Low,
    /// `high`.
    High,
    /// `auto`.
    Auto,
}

impl ImageDetail {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Auto => "auto",
        }
    }
}

/// Which `429` replies move a request to its next service tier.
///
/// A reply moves it when its `error.code` is among `codes` or its `error.message` starts with a
/// non-empty `message_prefix`, read from at most [`Self::ERROR_HEAD_BYTES`] and never kept.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TierFallback {
    /// Error codes that move the request on.
    pub codes: Vec<String>,
    /// The start of an error message that moves the request on; empty matches none.
    pub message_prefix: String,
}

impl TierFallback {
    /// The most bytes of a `429` reply read to match it.
    pub const ERROR_HEAD_BYTES: u64 = 16 << 10;

    /// A fallback on `codes` or on messages starting with `message_prefix`.
    #[must_use]
    pub fn new(codes: Vec<String>, message_prefix: impl Into<String>) -> Self {
        Self { codes, message_prefix: message_prefix.into() }
    }

    pub(super) fn matches(&self, head: &[u8]) -> bool {
        let Ok(reply) = serde_json::from_slice::<Value>(head) else { return false };
        let Some(error) = reply.get("error") else { return false };
        let code = error.get("code").and_then(Value::as_str);
        let message = error.get("message").and_then(Value::as_str);
        code.is_some_and(|code| self.codes.iter().any(|listed| listed == code))
            || (!self.message_prefix.is_empty()
                && message.is_some_and(|message| message.starts_with(&self.message_prefix)))
    }
}

impl fmt::Debug for TierFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TierFallback")
            .field("codes", &self.codes)
            .field("message_prefix", &format_args!("<{} bytes>", self.message_prefix.len()))
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct Template {
    pub(super) fields: Map<String, Value>,
    pub(super) prompt: String,
    pub(super) layout: MessageLayout,
    pub(super) detail: Option<ImageDetail>,
}

impl Template {
    pub(super) fn body(&self, image: &OcrImage, tier: Option<&str>) -> Vec<u8> {
        let url = format!("data:{};base64,{}", image.media_type, STANDARD.encode(&image.bytes));
        let mut source = Map::new();
        source.insert("url".into(), url.into());
        if let Some(detail) = self.detail {
            source.insert("detail".into(), detail.as_str().into());
        }
        let image = json!({ "type": "image_url", "image_url": source });
        let text = json!({ "type": "text", "text": self.prompt });
        let user = |content: Value| json!({ "role": "user", "content": content });
        let instruction = |role: &str| json!({ "role": role, "content": self.prompt });
        let messages = match self.layout {
            MessageLayout::ImageThenText => json!([user(json!([image, text]))]),
            MessageLayout::TextThenImage => json!([user(json!([text, image]))]),
            MessageLayout::DeveloperThenImage => {
                json!([instruction("developer"), user(json!([image]))])
            }
            MessageLayout::SystemThenImage => json!([instruction("system"), user(json!([image]))]),
        };
        let mut body = self.fields.clone();
        body.insert("messages".into(), messages);
        if let Some(tier) = tier {
            body.insert("service_tier".into(), tier.into());
        }
        Value::Object(body).to_string().into_bytes()
    }
}

pub(super) struct Reply {
    pub(super) answer: Result<String, OcrFailureKind>,
    pub(super) usage: Option<TokenUsage>,
    pub(super) tier: Option<u8>,
}

pub(super) fn reply(body: &[u8], answer_model: Option<&str>, tiers: &[String]) -> Reply {
    let Ok(reply) = serde_json::from_slice::<Value>(body) else {
        return Reply { answer: Err(OcrFailureKind::Protocol), usage: None, tier: None };
    };
    let tier = reply
        .get("service_tier")
        .and_then(Value::as_str)
        .and_then(|tier| tiers.iter().position(|listed| listed == tier))
        .and_then(|index| u8::try_from(index).ok());
    Reply { answer: answer(&reply, answer_model), usage: usage(&reply), tier }
}

/// The Markdown of the first choice when it came from `answer_model`, carries no refusal and
/// finished with `stop`, a leading `<think>` block and the whitespace after it dropped.
fn answer(reply: &Value, answer_model: Option<&str>) -> Result<String, OcrFailureKind> {
    if answer_model.is_some_and(|model| reply.get("model").and_then(Value::as_str) != Some(model)) {
        return Err(OcrFailureKind::WrongModel);
    }
    let choice = reply.get("choices").and_then(|choices| choices.get(0));
    let message = choice.and_then(|choice| choice.get("message")).filter(|m| m.is_object());
    let (Some(choice), Some(message)) = (choice, message) else {
        return Err(OcrFailureKind::Protocol);
    };
    if message.get("refusal").is_some_and(|refusal| !refusal.is_null()) {
        return Err(OcrFailureKind::Refused);
    }
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => {}
        Some("content_filter") => return Err(OcrFailureKind::Refused),
        _ => return Err(OcrFailureKind::Unfinished),
    }
    let text = message.get("content").and_then(Value::as_str).ok_or(OcrFailureKind::NoText)?;
    let Some(thought) = text.trim_start().strip_prefix("<think>") else {
        return Ok(text.to_owned());
    };
    let (_, after) = thought.split_once("</think>").ok_or(OcrFailureKind::NoText)?;
    Ok(after.trim_start().to_owned())
}

/// The reply's token counts; `None` when any is missing, not a non-negative integer, or more
/// prompt tokens are cached than were sent.
fn usage(reply: &Value) -> Option<TokenUsage> {
    let usage = reply.get("usage")?;
    let prompt = usage.get("prompt_tokens")?.as_u64()?;
    let output = usage.get("completion_tokens")?.as_u64()?;
    let cached = match usage.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens")) {
        None | Some(Value::Null) => 0,
        Some(cached) => cached.as_u64()?,
    };
    Some(TokenUsage { input_uncached: prompt.checked_sub(cached)?, input_cached: cached, output })
}

/// A `Retry-After` value: delay-seconds, saturating, or an IMF-fixdate.
pub(super) fn retry_after(value: &str) -> Option<RetryAfter> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(RetryAfter::Seconds(value.parse().unwrap_or(u64::MAX)));
    }
    http_date(value).map(RetryAfter::At)
}

/// The longer of two waits; a wait of the other form replaces the one kept.
pub(super) fn longer(kept: Option<RetryAfter>, seen: Option<RetryAfter>) -> Option<RetryAfter> {
    match (kept, seen) {
        (Some(RetryAfter::Seconds(a)), Some(RetryAfter::Seconds(b))) => {
            Some(RetryAfter::Seconds(a.max(b)))
        }
        (Some(RetryAfter::At(a)), Some(RetryAfter::At(b))) => Some(RetryAfter::At(a.max(b))),
        (kept, None) => kept,
        (_, seen) => seen,
    }
}

const DAY_NAMES: [&str; 7] = ["Mon,", "Tue,", "Wed,", "Thu,", "Fri,", "Sat,", "Sun,"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const SECONDS_PER_DAY: i64 = 86_400;
const DAYS_TO_UNIX_EPOCH: i64 = 719_468;

/// An IMF-fixdate (RFC 9110 5.6.7), such as `Sun, 06 Nov 1994 08:49:37 GMT`; one before 1970
/// reads as 1970 and a leap second as the second before it.
fn http_date(value: &str) -> Option<SystemTime> {
    let fields: [&str; 6] = value.split(' ').collect::<Vec<_>>().try_into().ok()?;
    let [day_name, day, month, year, time, zone] = fields;
    let clock: [&str; 3] = time.split(':').collect::<Vec<_>>().try_into().ok()?;
    let [hour, minute, second] = clock.map(|part| digits(part, 2));
    let (hour, minute, second, day, year) =
        (hour?, minute?, second?, digits(day, 2)?, digits(year, 4)?);
    let month = i64::try_from(MONTHS.iter().position(|name| *name == month)? + 1).ok()?;
    if zone != "GMT"
        || !DAY_NAMES.contains(&day_name)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let clock = hour * 3600 + minute * 60 + second.min(59);
    let seconds = days_from_civil(year, month, day) * SECONDS_PER_DAY + clock;
    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
}

fn digits(text: &str, width: usize) -> Option<i64> {
    (text.len() == width && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Hinnant's `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - DAYS_TO_UNIX_EPOCH
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;

    fn stopped(content: &Value, finish: &str) -> Value {
        json!({ "choices": [{ "message": { "content": content }, "finish_reason": finish }] })
    }

    fn answered(reply: &Value) -> Result<String, OcrFailureKind> {
        self::reply(reply.to_string().as_bytes(), None, &[]).answer
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
            let read = read.map(str::to_owned).ok_or(OcrFailureKind::NoText);
            assert_eq!(answered(&stopped(&json!(text), "stop")), read, "{text}");
        }
    }

    #[test]
    fn each_reply_that_holds_no_answer_names_why() {
        let refused =
            json!({ "choices": [{ "message": { "refusal": "no" }, "finish_reason": "stop" }] });
        let unfinished = json!({ "choices": [{ "message": { "content": "# A" } }] });
        let unwrapped = json!({ "choices": [{ "message": "# A", "finish_reason": "stop" }] });
        for (reply, kind) in [
            (stopped(&json!("# A"), "length"), OcrFailureKind::Unfinished),
            (unfinished, OcrFailureKind::Unfinished),
            (stopped(&json!("# A"), "content_filter"), OcrFailureKind::Refused),
            (refused, OcrFailureKind::Refused),
            (stopped(&Value::Null, "stop"), OcrFailureKind::NoText),
            (stopped(&json!(["# A"]), "stop"), OcrFailureKind::NoText),
            (json!({}), OcrFailureKind::Protocol),
            (json!({ "choices": [] }), OcrFailureKind::Protocol),
            (unwrapped, OcrFailureKind::Protocol),
        ] {
            assert_eq!(answered(&reply), Err(kind), "{reply}");
        }
        for body in [&b"not json"[..], b""] {
            assert_eq!(reply(body, None, &[]).answer, Err(OcrFailureKind::Protocol));
        }
    }

    #[test]
    fn usage_with_a_bool_a_negative_a_fraction_or_too_many_cached_tokens_is_none() {
        let read = |usage: Value| self::usage(&json!({ "usage": usage }));
        assert_eq!(
            read(json!({ "prompt_tokens": 3, "completion_tokens": 2 })),
            Some(TokenUsage { input_uncached: 3, input_cached: 0, output: 2 })
        );
        let cached = |cached: Value| {
            json!({
                "prompt_tokens": 3,
                "completion_tokens": 2,
                "prompt_tokens_details": { "cached_tokens": cached }
            })
        };
        for usage in [
            json!({ "prompt_tokens": true, "completion_tokens": 2 }),
            json!({ "prompt_tokens": -1, "completion_tokens": 2 }),
            json!({ "prompt_tokens": 3.0, "completion_tokens": 2 }),
            json!({ "prompt_tokens": 3 }),
            cached(json!(4)),
            cached(json!(false)),
            cached(json!(-1)),
        ] {
            assert_eq!(read(usage.clone()), None, "{usage}");
        }
    }

    fn image() -> OcrImage {
        OcrImage { bytes: Arc::from(&b"\x89PNG"[..]), media_type: "image/png", dpi: None }
    }

    #[test]
    fn a_body_holds_its_fields_tier_and_one_data_uri_in_its_layout() {
        let mut fields = Map::new();
        fields.insert("model".into(), json!("m"));
        fields.insert("top_p".into(), json!(0.5));
        let mut template = Template {
            fields,
            prompt: "Read it.".into(),
            layout: MessageLayout::ImageThenText,
            detail: None,
        };
        let body: Value = serde_json::from_slice(&template.body(&image(), None)).unwrap();
        let parts = &body["messages"][0]["content"];
        assert_eq!(
            (&body["model"], &body["top_p"], &body["messages"][0]["role"]),
            (&json!("m"), &json!(0.5), &json!("user"))
        );
        assert_eq!(parts[0]["image_url"], json!({ "url": "data:image/png;base64,iVBORw==" }));
        assert_eq!(parts[1], json!({ "type": "text", "text": "Read it." }));
        assert_eq!(body.get("service_tier"), None);
        template.layout = MessageLayout::TextThenImage;
        let body: Value = serde_json::from_slice(&template.body(&image(), Some("flex"))).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], json!("text"));
        assert_eq!(body["service_tier"], json!("flex"));
        template.detail = Some(ImageDetail::High);
        let source = json!({ "url": "data:image/png;base64,iVBORw==", "detail": "high" });
        for (layout, role) in [
            (MessageLayout::DeveloperThenImage, "developer"),
            (MessageLayout::SystemThenImage, "system"),
        ] {
            template.layout = layout;
            let body: Value = serde_json::from_slice(&template.body(&image(), None)).unwrap();
            let user = json!({ "role": "user", "content": [
                { "type": "image_url", "image_url": source }
            ] });
            assert_eq!(body["messages"], json!([{ "role": role, "content": "Read it." }, user]));
        }
    }

    #[test]
    fn a_fallback_matches_a_listed_code_or_a_message_prefix_only() {
        let fallback = TierFallback::new(vec!["flex_busy".into()], "Flex is");
        let error = |code: Value, message: &str| {
            json!({ "error": { "code": code, "message": message } }).to_string().into_bytes()
        };
        assert!(fallback.matches(&error(json!("flex_busy"), "other")));
        assert!(fallback.matches(&error(Value::Null, "Flex is unavailable")));
        assert!(!fallback.matches(&error(json!("rate_limit"), "Too many requests")));
        assert!(!fallback.matches(b"{\"error\": {\"message\": \"Flex is"));
        assert!(!TierFallback::new(Vec::new(), "").matches(&error(Value::Null, "Flex is")));
        let shown = format!("{fallback:?}");
        assert!(shown.contains("flex_busy") && !shown.contains("Flex is"), "{shown}");
    }

    #[test]
    fn retry_after_reads_seconds_or_an_imf_fixdate_and_keeps_the_longer() {
        let at = |seconds| RetryAfter::At(UNIX_EPOCH + Duration::from_secs(seconds));
        for (value, read) in [
            (" 120 ", Some(RetryAfter::Seconds(120))),
            ("0", Some(RetryAfter::Seconds(0))),
            ("99999999999999999999999", Some(RetryAfter::Seconds(u64::MAX))),
            ("Sun, 06 Nov 1994 08:49:37 GMT", Some(at(784_111_777))),
            ("Thu, 01 Jan 1970 00:00:00 GMT", Some(at(0))),
            ("Thu, 29 Feb 2024 23:59:60 GMT", Some(at(1_709_251_199))),
            ("Fri, 31 Dec 9999 23:59:60 GMT", Some(at(253_402_300_799))),
            ("Wed, 31 Dec 1969 23:59:59 GMT", Some(at(0))),
            ("Wed, 29 Feb 2023 00:00:00 GMT", None),
            ("Sun, 06 Nov 1994 08:49:37 UTC", None),
            ("Sunday, 06-Nov-94 08:49:37 GMT", None),
            ("Sun, 6 Nov 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 1994 24:00:00 GMT", None),
            ("-1", None),
            ("", None),
        ] {
            assert_eq!(retry_after(value), read, "{value}");
        }
        let seconds = RetryAfter::Seconds;
        assert_eq!(longer(Some(seconds(3)), Some(seconds(9))), Some(seconds(9)));
        assert_eq!(longer(Some(seconds(9)), None), Some(seconds(9)));
        assert_eq!(longer(Some(at(9)), Some(at(3))), Some(at(9)));
        assert_eq!(longer(Some(seconds(9)), Some(at(3))), Some(at(3)));
        assert_eq!(longer(None, Some(seconds(1))), Some(seconds(1)));
    }

    proptest! {
        #[test]
        fn any_body_reads_without_panicking(body in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _: Reply = reply(&body, Some("m"), &["flex".to_owned()]);
        }

        #[test]
        fn any_header_reads_without_panicking(value in ".{0,40}") {
            let _: Option<RetryAfter> = retry_after(&value);
        }

        #[test]
        fn any_stopped_text_keeps_what_follows_its_think_block(text in ".{0,64}") {
            let read = answered(&stopped(&json!(format!("<think>{text}</think>{text}")), "stop"));
            if !text.contains("</think>") {
                prop_assert_eq!(read.ok(), Some(text.trim_start().to_owned()));
            }
        }
    }
}
