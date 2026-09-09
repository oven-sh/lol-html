mod to_token;

pub(crate) use self::to_token::{ToToken, ToTokenResult};

use bitflags::bitflags;
bitflags! {
    /// Which token kinds the [`TransformController`](crate::transform::TransformController)
    /// wants delivered as [`Token`](crate::transform::Token)s. `NEXT_START_TAG` /
    /// `NEXT_END_TAG` apply to the tag being decided on and are cleared once
    /// it has been delivered; the others stay in effect until changed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TokenCaptureFlags: u8 {
        /// Text, as decoded [`TextChunk`](crate::html_content::TextChunk)s.
        const TEXT = 0b0000_0001;
        /// Comments.
        const COMMENTS = 0b0000_0010;
        /// The start tag just reported, with its attributes.
        const NEXT_START_TAG = 0b0000_0100;
        /// The end tag just reported.
        const NEXT_END_TAG = 0b0000_1000;
        /// Doctypes.
        const DOCTYPES = 0b0001_0000;
        /// Text as the raw, undecoded byte runs the tokenizer sees, through
        /// [`TransformController::handle_raw_text`](crate::transform::TransformController::handle_raw_text)
        /// instead of as [`TextChunk`](crate::html_content::TextChunk)
        /// tokens: no decoding to UTF-8, no `last_in_text_node` bookkeeping,
        /// nothing to rewrite. `TEXT` takes precedence if both are set.
        const RAW_TEXT = 0b0010_0000;
    }
}
