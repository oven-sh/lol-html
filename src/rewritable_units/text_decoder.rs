use crate::base::{Bytes, SharedEncoding, SourceLocation, Spanned};
use crate::rewriter::RewritingError;
use encoding_rs::{CoderResult, Decoder, Encoding, UTF_8};

const DEFAULT_BUFFER_LEN: usize = if cfg!(test) { 13 } else { 1024 };

pub(crate) struct TextDecoder {
    encoding: SharedEncoding,
    pending_source_location_bytes_start: usize,
    pending_text_streaming_decoder: Option<Decoder>,
    text_buffer: String,
    /// Where the current `feed_text` call was when a text handler suspended
    /// (`None` otherwise). Recorded *only* on the error path so the success
    /// path pays nothing.
    suspended: Option<TextFeedSuspension>,
    /// The incomplete UTF-8 sequence the streaming decoder consumed at the
    /// end of a non-final feed and keeps in its own state. These are source
    /// bytes of the chunk that will show their character (or its
    /// replacement), not of the chunk that was being decoded when they were
    /// consumed.
    held: HeldBytes,
    /// Source bytes of the chunk a text handler is suspended on, when the
    /// chunk's text is a lossy decoding of them. Recorded only on the error
    /// path, like `suspended`.
    parked_lossy_source: Option<Vec<u8>>,
}

/// Where a [`TextDecoder::feed_text`] call was when the text handler for one
/// of its chunks suspended. Lets [`TextDecoder::resume_feed`] finish decoding
/// the rest of that text lexeme once the parked chunk completes. The
/// undecoded tail is copied because it pointed into the `write()` call's
/// input buffer, which is gone by the time the resume runs.
pub(crate) struct TextFeedSuspension {
    remaining: Vec<u8>,
    next_source_location_bytes_start: usize,
    last_in_text_node: bool,
    /// `true` if the chunk that suspended was this feed's final one; the
    /// resume then only has to run the loop's epilogue.
    finished: bool,
}

/// Receives the chunks of decoded text.
pub(crate) trait TextSink {
    /// A chunk whose `text` is, byte for byte, the input it was decoded from
    /// (in a UTF-8 document), or any chunk of a document in another encoding.
    fn text(
        &mut self,
        text: &str,
        last_in_text_node: bool,
        encoding: &'static Encoding,
        source_location: SourceLocation,
    ) -> Result<(), RewritingError>;

    /// Like [`Self::text`], for a chunk of a UTF-8 document whose `text` has
    /// U+FFFD in place of malformed input. `source[0]` followed by
    /// `source[1]` are the bytes it was decoded from.
    fn lossy_text(
        &mut self,
        text: &str,
        source: [&[u8]; 2],
        last_in_text_node: bool,
        encoding: &'static Encoding,
        source_location: SourceLocation,
    ) -> Result<(), RewritingError>;
}

pub(crate) type OutputHandlerCallback<'tmp> = dyn TextSink + 'tmp;

/// An incomplete UTF-8 sequence: a lead byte and up to two continuation bytes.
#[derive(Copy, Clone)]
struct HeldBytes {
    bytes: [u8; 3],
    len: u8,
}

impl HeldBytes {
    const EMPTY: Self = Self {
        bytes: [0; 3],
        len: 0,
    };

    #[inline]
    fn as_slice(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or_default()
    }

    /// Appends `more`. An incomplete sequence never has more than 3 bytes, so
    /// there is always room.
    fn extend(&mut self, more: &[u8]) {
        for &byte in more {
            if let Some(slot) = self.bytes.get_mut(usize::from(self.len)) {
                *slot = byte;
                self.len += 1;
            }
        }
    }
}

impl TextDecoder {
    #[inline]
    #[must_use]
    pub fn new(encoding: SharedEncoding) -> Self {
        Self {
            pending_source_location_bytes_start: 0,
            encoding,
            pending_text_streaming_decoder: None,
            // this will be later initialized to DEFAULT_BUFFER_LEN,
            // because encoding_rs wants a slice
            text_buffer: String::new(),
            suspended: None,
            held: HeldBytes::EMPTY,
            parked_lossy_source: None,
        }
    }

    #[inline]
    pub fn flush_pending(
        &mut self,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        if self.pending_text_streaming_decoder.is_some() {
            self.feed_text(
                Spanned::new(self.pending_source_location_bytes_start, Bytes::new(&[])),
                true,
                output_handler,
            )?;
        }
        Ok(())
    }

    /// The feed state recorded when a text handler suspended, if any.
    #[inline]
    pub fn take_suspended(&mut self) -> Option<TextFeedSuspension> {
        self.suspended.take()
    }

    /// The source bytes of the chunk a text handler is suspended on, if its
    /// text is a lossy decoding of them.
    #[inline]
    pub fn take_parked_lossy_source(&mut self) -> Option<Vec<u8>> {
        self.parked_lossy_source.take()
    }

    /// Continues a `feed_text` call that a text handler suspended, starting
    /// right after the chunk that suspended.
    pub fn resume_feed(
        &mut self,
        suspension: TextFeedSuspension,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        if suspension.finished {
            // The chunk that suspended was this feed's final one: only the
            // loop epilogue is left.
            if suspension.last_in_text_node {
                self.pending_text_streaming_decoder = None;
            } else {
                self.pending_source_location_bytes_start =
                    suspension.next_source_location_bytes_start;
            }
            return Ok(());
        }

        self.feed_decoder_loop(
            &suspension.remaining,
            suspension.next_source_location_bytes_start,
            suspension.last_in_text_node,
            output_handler,
        )
    }

    #[inline(never)]
    pub fn feed_text(
        &mut self,
        input_span: Spanned<Bytes<'_>>,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        let mut raw_input = input_span.as_slice();
        let mut next_source_location_bytes_start = input_span.source_location().bytes().start;

        let encoding = self.encoding.get();

        if let Some((utf8_text, rest)) = self.split_utf8_start(raw_input, encoding) {
            // No streaming decoder is pending, so none holds a sequence.
            debug_assert_eq!(self.held.len, 0);
            raw_input = rest;
            let really_last = last_in_text_node && rest.is_empty();

            let source_location =
                SourceLocation::from_start_len(next_source_location_bytes_start, utf8_text.len());
            next_source_location_bytes_start = source_location.bytes().end;

            let res = output_handler.text(utf8_text, really_last, encoding, source_location);
            if res.is_err() {
                self.suspended = Some(TextFeedSuspension {
                    remaining: rest.to_vec(),
                    next_source_location_bytes_start,
                    last_in_text_node,
                    finished: really_last,
                });
            }
            res?;

            if really_last {
                debug_assert!(self.pending_text_streaming_decoder.is_none());
                return Ok(());
            }
        }

        self.feed_decoder_loop(
            raw_input,
            next_source_location_bytes_start,
            last_in_text_node,
            output_handler,
        )
    }

    /// The streaming-decoder tail of [`Self::feed_text`], also the re-entry
    /// point for [`Self::resume_feed`].
    #[inline(always)]
    fn feed_decoder_loop(
        &mut self,
        raw_input: &[u8],
        next_source_location_bytes_start: usize,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        if (raw_input.len() | usize::from(self.held.len)) == 0 {
            // Nothing gets consumed and no held sequence gets flushed: the
            // text this emits (if any) has no source bytes to account for.
            // This is the call that closes every text node whose text went
            // out through the fast path, so it stays free of the bookkeeping.
            self.plain_decoder_loop(
                raw_input,
                next_source_location_bytes_start,
                last_in_text_node,
                output_handler,
            )
        } else {
            self.accounting_decoder_loop(
                raw_input,
                next_source_location_bytes_start,
                last_in_text_node,
                output_handler,
            )
        }
    }

    #[inline(never)]
    fn plain_decoder_loop(
        &mut self,
        raw_input: &[u8],
        next_source_location_bytes_start: usize,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        self.decoder_loop::<false>(
            raw_input,
            next_source_location_bytes_start,
            last_in_text_node,
            output_handler,
        )
    }

    /// [`Self::plain_decoder_loop`] that also keeps track of which input
    /// bytes each chunk of text came from.
    #[inline(never)]
    fn accounting_decoder_loop(
        &mut self,
        raw_input: &[u8],
        next_source_location_bytes_start: usize,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        self.decoder_loop::<true>(
            raw_input,
            next_source_location_bytes_start,
            last_in_text_node,
            output_handler,
        )
    }

    #[inline(always)]
    fn decoder_loop<const ACCOUNT: bool>(
        &mut self,
        mut raw_input: &[u8],
        mut next_source_location_bytes_start: usize,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        let encoding = self.encoding.get();

        if self.pending_text_streaming_decoder.is_none() && self.text_buffer.is_empty() {
            // repeat() avoids utf8 check comapred to `String::from_utf8(vec![0; len])`
            self.text_buffer = "\0".repeat(DEFAULT_BUFFER_LEN);
        }
        let decoder = self
            .pending_text_streaming_decoder
            .get_or_insert_with(|| encoding.new_decoder_without_bom_handling());
        // Only UTF-8 text reports lossy chunks: there the decoded text of
        // every other chunk is its source, so nothing else has to be kept.
        let account = ACCOUNT && decoder.encoding() == UTF_8;

        loop {
            let buffer = self.text_buffer.as_mut_str();
            let (status, read, written, had_replacements) =
                decoder.decode_to_str(raw_input, buffer, last_in_text_node);

            let finished_decoding = status == CoderResult::InputEmpty;
            let source_location =
                SourceLocation::from_start_len(next_source_location_bytes_start, read);
            next_source_location_bytes_start = source_location.bytes().end;

            // The source bytes of the text this call wrote, if the text is not
            // a faithful copy of them. The bookkeeping is settled here, before
            // the handler runs: a handler that suspends leaves through `?`.
            let lossy_source = if account {
                account_source_bytes(
                    &mut self.held,
                    raw_input.get(..read).unwrap_or_default(),
                    finished_decoding && !last_in_text_node,
                    had_replacements,
                )
            } else {
                None
            };

            if written > 0 || last_in_text_node {
                // the last call to feed_text() may make multiple calls to output_handler,
                // but only one call to output_handler can be *the* last one.
                let really_last = last_in_text_node && finished_decoding;

                // this will always be in bounds, but unwrap_or_default optimizes better
                let text = buffer.get(..written).unwrap_or_default();
                let res = match lossy_source {
                    None => output_handler.text(text, really_last, encoding, source_location),
                    Some((held_before, body)) => emit_lossy(
                        &mut self.parked_lossy_source,
                        output_handler,
                        text,
                        [held_before.as_slice(), body],
                        really_last,
                        encoding,
                        source_location,
                    ),
                };
                if res.is_err() {
                    self.suspended = Some(TextFeedSuspension {
                        remaining: raw_input.get(read..).unwrap_or_default().to_vec(),
                        next_source_location_bytes_start,
                        last_in_text_node,
                        finished: finished_decoding,
                    });
                }
                res?;
            }

            if finished_decoding {
                if last_in_text_node {
                    self.pending_text_streaming_decoder = None;
                } else {
                    self.pending_source_location_bytes_start = next_source_location_bytes_start;
                }
                return Ok(());
            }
            raw_input = raw_input.get(read..).unwrap_or_default();
        }
    }

    /// Fast path for UTF-8 or ASCII prefix
    ///
    /// Returns UTF-8 text to emit + remaining bytes, or `None` if the fast path is not available
    #[inline]
    fn split_utf8_start<'i>(
        &self,
        raw_input: &'i [u8],
        encoding: &'static Encoding,
    ) -> Option<(&'i str, &'i [u8])> {
        // Can't use the fast path if the decoder may have buffered some bytes
        if self.pending_text_streaming_decoder.is_some() {
            return None;
        }

        let text_or_len = if encoding == UTF_8 {
            std::str::from_utf8(raw_input).map_err(|err| err.valid_up_to())
        } else {
            debug_assert!(encoding.is_ascii_compatible());
            Err(Encoding::ascii_valid_up_to(raw_input))
        };

        match text_or_len {
            Ok(utf8_text) => Some((utf8_text, &[][..])),
            Err(valid_up_to) => {
                // The slow path buffers 1KB, and even though this shouldn't matter,
                // it is an observable behavior, and it makes bugs worse for text handlers
                // that assume they'll get only a single chunk.
                if valid_up_to != raw_input.len() && valid_up_to < DEFAULT_BUFFER_LEN {
                    return None;
                }

                let (text, rest) = raw_input.split_at_checked(valid_up_to)?;
                Some((std::str::from_utf8(text).ok()?, rest))
            }
        }
    }
}

/// Hands a chunk whose text is a lossy decoding of `source` to the sink, and
/// keeps a copy of `source` if the sink fails: a suspending handler parks the
/// chunk, and `source` points into the `write()` call's input buffer, which is
/// gone by the time the chunk resumes.
#[cold]
#[inline(never)]
fn emit_lossy(
    parked_lossy_source: &mut Option<Vec<u8>>,
    output_handler: &mut OutputHandlerCallback<'_>,
    text: &str,
    source: [&[u8]; 2],
    really_last: bool,
    encoding: &'static Encoding,
    source_location: SourceLocation,
) -> Result<(), RewritingError> {
    let res = output_handler.lossy_text(text, source, really_last, encoding, source_location);
    if res.is_err() {
        *parked_lossy_source = Some(source.concat());
    }
    res
}

/// Moves the source-byte bookkeeping past one `decode_to_str` call that
/// consumed `consumed` (UTF-8 only). `may_hold` says whether the decoder can
/// have kept an incomplete sequence: it does so only when it ran out of input
/// and more input may follow.
///
/// Returns the source bytes of the text that call wrote, when the text is not
/// a faithful copy of them: the sequence held from earlier calls, then the
/// part of `consumed` that got decoded.
#[inline]
fn account_source_bytes<'i>(
    held: &mut HeldBytes,
    consumed: &'i [u8],
    may_hold: bool,
    had_replacements: bool,
) -> Option<(HeldBytes, &'i [u8])> {
    // The common case: no sequence was held, none was replaced, and none is
    // held now either, because the decoder did not stop for lack of input or
    // because the input it consumed ends in ASCII.
    let holds_nothing = consumed.last().is_some_and(u8::is_ascii) || !may_hold;
    if held.len == 0 && !had_replacements && holds_nothing {
        return None;
    }
    account_source_bytes_slow(held, consumed, may_hold, had_replacements)
}

#[cold]
#[inline(never)]
fn account_source_bytes_slow<'i>(
    held: &mut HeldBytes,
    consumed: &'i [u8],
    may_hold: bool,
    had_replacements: bool,
) -> Option<(HeldBytes, &'i [u8])> {
    let before = *held;
    let tail_len = if may_hold {
        utf8_incomplete_tail_len(before.as_slice(), consumed)
    } else {
        0
    };
    let Some((body, tail)) = consumed
        .len()
        .checked_sub(tail_len)
        .and_then(|body_len| consumed.split_at_checked(body_len))
    else {
        // The incomplete sequence starts in the bytes held so far: all of
        // `consumed` extends it, and no text came out of it.
        held.extend(consumed);
        return None;
    };
    *held = HeldBytes::EMPTY;
    held.extend(tail);
    had_replacements.then_some((before, body))
}

/// Length of the incomplete, so far well-formed, UTF-8 sequence at the end of
/// `a` followed by `b` (0 if there is none): the bytes a WHATWG UTF-8 decoder
/// keeps in its state once it has consumed them and expects more input.
fn utf8_incomplete_tail_len(a: &[u8], b: &[u8]) -> usize {
    let mut bytes_from_end = b.iter().rev().chain(a.iter().rev()).copied();
    // The byte that follows the lead byte, once the scan reaches the lead.
    let mut second = None;
    for len in 1..=3 {
        let Some(byte) = bytes_from_end.next() else {
            return 0;
        };
        let (sequence_len, valid_second) = match byte {
            0x80..=0xBF => {
                second = Some(byte);
                continue;
            }
            0xC2..=0xDF => (2, 0x80..=0xBF),
            0xE0 => (3, 0xA0..=0xBF),
            0xED => (3, 0x80..=0x9F),
            0xE1..=0xEF => (3, 0x80..=0xBF),
            0xF0 => (4, 0x90..=0xBF),
            0xF4 => (4, 0x80..=0x8F),
            0xF1..=0xF3 => (4, 0x80..=0xBF),
            _ => return 0,
        };
        return if len < sequence_len && second.is_none_or(|s| valid_second.contains(&s)) {
            len
        } else {
            0
        };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewritable_units::test_utils::{Rng, random_cuts, random_text};
    use crate::rewriter::AsciiCompatibleEncoding;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Chunk {
        text: String,
        last: bool,
        /// `Some` if the chunk came through [`TextSink::lossy_text`].
        source: Option<Vec<u8>>,
    }

    /// Records the chunks and fails like a suspending handler on some of them.
    struct Recorder {
        chunks: Vec<Chunk>,
        rng: Rng,
        suspend_one_in: u64,
    }

    impl Recorder {
        fn record(&mut self, chunk: Chunk) -> Result<(), RewritingError> {
            self.chunks.push(chunk);
            if self.suspend_one_in != 0 && self.rng.below(self.suspend_one_in) == 0 {
                Err(RewritingError::Suspended)
            } else {
                Ok(())
            }
        }
    }

    impl TextSink for Recorder {
        fn text(
            &mut self,
            text: &str,
            last: bool,
            _: &'static Encoding,
            _: SourceLocation,
        ) -> Result<(), RewritingError> {
            self.record(Chunk {
                text: text.to_owned(),
                last,
                source: None,
            })
        }

        fn lossy_text(
            &mut self,
            text: &str,
            source: [&[u8]; 2],
            last: bool,
            _: &'static Encoding,
            _: SourceLocation,
        ) -> Result<(), RewritingError> {
            self.record(Chunk {
                text: text.to_owned(),
                last,
                source: Some(source.concat()),
            })
        }
    }

    /// Feeds one text node the way the dispatcher does: every lexeme with
    /// `last_in_text_node == false`, then `flush_pending`. After a suspension
    /// it takes the parked source and resumes the feed.
    fn decode_text_node(feeds: &[&[u8]], encoding: &'static Encoding, rec: &mut Recorder) {
        let encoding = AsciiCompatibleEncoding::new(encoding).unwrap();
        let mut decoder = TextDecoder::new(SharedEncoding::new(encoding));

        fn settle(
            decoder: &mut TextDecoder,
            rec: &mut Recorder,
            mut res: Result<(), RewritingError>,
        ) {
            while let Err(e) = res {
                assert!(matches!(e, RewritingError::Suspended));
                let parked = decoder.take_parked_lossy_source();
                assert_eq!(parked, rec.chunks.last().unwrap().source);
                let feed = decoder.take_suspended().unwrap();
                res = decoder.resume_feed(feed, rec);
            }
            assert!(decoder.take_parked_lossy_source().is_none());
            assert!(decoder.take_suspended().is_none());
        }

        let mut start = 0;
        for feed in feeds {
            let res = decoder.feed_text(Spanned::new(start, Bytes::new(feed)), false, rec);
            settle(&mut decoder, rec, res);
            start += feed.len();
        }
        let res = decoder.flush_pending(rec);
        settle(&mut decoder, rec, res);
        assert!(decoder.pending_text_streaming_decoder.is_none());
        assert_eq!(decoder.held.len, 0);
    }

    /// The `(text, last_in_text_node)` sequence of the decoder as it was
    /// before it kept track of source bytes: the fast path for a well-formed
    /// prefix, then `decode_to_str` into a `DEFAULT_BUFFER_LEN` buffer.
    fn chunks_before_source_tracking(feeds: &[&[u8]]) -> Vec<(String, bool)> {
        let mut chunks = vec![];
        let mut decoder: Option<Decoder> = None;
        let mut buffer = "\0".repeat(DEFAULT_BUFFER_LEN);

        let mut feed = |mut input: &[u8], last: bool| {
            if decoder.is_none() {
                if last {
                    // `flush_pending` with no text pending
                    return;
                }
                let valid_up_to = match std::str::from_utf8(input) {
                    Ok(_) => Some(input.len()),
                    Err(e) if e.valid_up_to() >= DEFAULT_BUFFER_LEN => Some(e.valid_up_to()),
                    Err(_) => None,
                };
                if let Some(valid_up_to) = valid_up_to {
                    let (text, rest) = input.split_at(valid_up_to);
                    let really_last = last && rest.is_empty();
                    chunks.push((String::from_utf8(text.to_vec()).unwrap(), really_last));
                    if really_last {
                        return;
                    }
                    input = rest;
                }
            }
            let streaming = decoder.get_or_insert_with(|| UTF_8.new_decoder_without_bom_handling());
            loop {
                let (status, read, written, _) = streaming.decode_to_str(input, &mut buffer, last);
                let finished = status == CoderResult::InputEmpty;
                if written > 0 || last {
                    chunks.push((buffer[..written].to_owned(), last && finished));
                }
                if finished {
                    if last {
                        decoder = None;
                    }
                    return;
                }
                input = &input[read..];
            }
        };

        for input in feeds {
            feed(input, false);
        }
        feed(&[], true);
        chunks
    }

    /// Checks that the chunks of one UTF-8 text node account for every input
    /// byte exactly once, in order, and that a chunk is lossy if and only if
    /// its text is not its source.
    fn assert_chunks_account_for(input: &[u8], chunks: &[Chunk]) {
        let mut rest = input;
        for chunk in chunks {
            let source = match &chunk.source {
                Some(source) => {
                    assert!(chunk.text.contains('\u{fffd}'), "{chunk:x?}");
                    assert_ne!(chunk.text.as_bytes(), &source[..], "{chunk:x?}");
                    assert_eq!(chunk.text, String::from_utf8_lossy(source), "{chunk:x?}");
                    &source[..]
                }
                None => chunk.text.as_bytes(),
            };
            let Some(after) = rest.strip_prefix(source) else {
                panic!("{chunk:x?} is not the source at {rest:x?} of {input:x?}");
            };
            rest = after;
        }
        assert!(rest.is_empty(), "{rest:x?} of {input:x?} is in no chunk");
    }

    #[test]
    fn incomplete_tail_len_is_what_a_decoder_holds() {
        // Every byte that starts, continues or breaks a sequence differently.
        const BYTES: [u8; 20] = [
            0x41, 0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC1, 0xC2, 0xDF, 0xE0, 0xE1, 0xED,
            0xEF, 0xF0, 0xF1, 0xF4, 0xF5, 0xFF,
        ];

        /// The same, read off the errors of a complete decode.
        fn expected(mut bytes: &[u8]) -> usize {
            loop {
                match std::str::from_utf8(bytes) {
                    Ok(_) => return 0,
                    Err(e) => match e.error_len() {
                        None => return bytes.len() - e.valid_up_to(),
                        Some(len) => bytes = &bytes[e.valid_up_to() + len..],
                    },
                }
            }
        }

        /// The same, asked of encoding_rs: flush the decoder and see what the
        /// bytes it still held turn into.
        fn holds_bytes(bytes: &[u8]) -> bool {
            let mut decoder = UTF_8.new_decoder_without_bom_handling();
            let mut buffer = [0u8; 64];
            let (status, read, ..) = decoder.decode_to_utf8(bytes, &mut buffer, false);
            assert_eq!((status, read), (CoderResult::InputEmpty, bytes.len()));
            let (.., written, had_replacements) = decoder.decode_to_utf8(&[], &mut buffer, true);
            assert_eq!(had_replacements, written == 3);
            had_replacements
        }

        let mut sequence = vec![];
        for len in 0..=4 {
            for mut n in 0..BYTES.len().pow(len) {
                sequence.clear();
                for _ in 0..len {
                    sequence.push(BYTES[n % BYTES.len()]);
                    n /= BYTES.len();
                }
                let expected = expected(&sequence);
                assert_eq!(expected != 0, holds_bytes(&sequence), "{sequence:x?}");
                for split in 0..=sequence.len() {
                    let (a, b) = sequence.split_at(split);
                    assert_eq!(utf8_incomplete_tail_len(a, b), expected, "{a:x?} {b:x?}");
                }
            }
        }
    }

    #[test]
    fn chunks_account_for_every_source_byte() {
        let nodes: u64 = if cfg!(miri) { 200 } else { 20_000 };
        let mut lossy_chunks = 0;
        let mut suspensions_possible = 0;

        for seed in 1..=nodes {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut input = vec![];
            let len = match rng.below(5) {
                0 => rng.below(4) as usize,
                1 => 40 + rng.below(120) as usize,
                _ => 1 + rng.below(30) as usize,
            };
            let damage = rng.below(4) != 0;
            random_text(&mut rng, len, damage, &mut input);

            let cuts = random_cuts(&mut rng, input.len());
            let mut feeds = vec![];
            let mut start = 0;
            for &end in &cuts {
                feeds.push(&input[start..end]);
                if rng.below(8) == 0 {
                    feeds.push(&[][..]);
                }
                start = end;
            }

            let expected = chunks_before_source_tracking(&feeds);

            for suspend_one_in in [0, 1, 3] {
                let mut rec = Recorder {
                    chunks: vec![],
                    rng: Rng(seed | 1),
                    suspend_one_in,
                };
                decode_text_node(&feeds, UTF_8, &mut rec);

                let sequence: Vec<_> = rec
                    .chunks
                    .iter()
                    .map(|c| (c.text.clone(), c.last))
                    .collect();
                assert_eq!(
                    sequence, expected,
                    "seed {seed}: {input:x?} cut at {cuts:?}"
                );
                assert_chunks_account_for(&input, &rec.chunks);

                lossy_chunks += rec.chunks.iter().filter(|c| c.source.is_some()).count();
                suspensions_possible += usize::from(suspend_one_in != 0);
            }
        }

        // The inputs do reach the paths this test is about.
        assert!(lossy_chunks > nodes as usize / 2, "{lossy_chunks}");
        assert!(suspensions_possible > 0);
    }

    #[test]
    fn an_empty_feed_keeps_the_held_sequence() {
        let mut rec = Recorder {
            chunks: vec![],
            rng: Rng(1),
            suspend_one_in: 0,
        };
        decode_text_node(
            &[b"a\xE2", b"", b"\x82", b"", b"\xACb\xE2", b""],
            UTF_8,
            &mut rec,
        );
        assert_chunks_account_for(b"a\xE2\x82\xACb\xE2", &rec.chunks);
        let texts: Vec<_> = rec.chunks.iter().map(|c| (&*c.text, c.last)).collect();
        assert_eq!(
            texts,
            [("a", false), ("\u{20ac}b", false), ("\u{fffd}", true)]
        );
    }

    #[test]
    fn well_formed_text_is_never_lossy() {
        for seed in 1..=2_000u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut input = vec![];
            let len = 1 + rng.below(80) as usize;
            random_text(&mut rng, len, false, &mut input);

            let cuts = random_cuts(&mut rng, input.len());
            let mut feeds = vec![];
            let mut start = 0;
            for &end in &cuts {
                feeds.push(&input[start..end]);
                start = end;
            }

            let mut rec = Recorder {
                chunks: vec![],
                rng: Rng(seed | 1),
                suspend_one_in: 2,
            };
            decode_text_node(&feeds, UTF_8, &mut rec);
            assert!(rec.chunks.iter().all(|c| c.source.is_none()), "{input:x?}");
            assert_chunks_account_for(&input, &rec.chunks);
        }
    }

    #[test]
    fn only_a_utf8_document_reports_lossy_chunks() {
        // 0x81 is unmapped in windows-1252 only by name: encoding_rs decodes
        // every byte there. Shift_JIS has malformed sequences.
        let mut rec = Recorder {
            chunks: vec![],
            rng: Rng(1),
            suspend_one_in: 0,
        };
        decode_text_node(&[b"a\x81\xFFb", b"\x93"], encoding_rs::SHIFT_JIS, &mut rec);
        assert!(rec.chunks.iter().any(|c| c.text.contains('\u{fffd}')));
        assert!(rec.chunks.iter().all(|c| c.source.is_none()));
    }
}
