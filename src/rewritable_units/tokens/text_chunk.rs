use super::Mutations;
use crate::base::SourceLocation;
use crate::errors::RewritingError;
use crate::html::TextType;
use crate::html_content::{ContentType, StreamingHandler, StreamingHandlerSink};
use crate::rewritable_units::StringChunk;
use encoding_rs::Encoding;
use std::any::Any;
use std::borrow::Cow;
use std::fmt::{self, Debug};

/// A fragment of an HTML text node. Beware: this is tricky to use.
///
/// ## The text is fragmented
///
/// The text chunks are **not** complete text DOM nodes. They are fragments of text nodes, split at arbitrary points.
///
/// Since the rewriter operates on a streaming input with minimal internal buffering, HTML
/// text node can be represented by multiple text chunks. The size of a chunk depends on multiple
/// parameters, such as decoding buffer size and input chunk size.
///
/// It is up to a user of the rewriter to buffer content of chunks to get the whole text node
/// content where desired. The last chunk in a text node can be determined by calling
/// [`last_in_text_node`] method of the chunk.
///
/// Note that in the sequence `"<span>red-<b>or</b>-blue</span>"` the `span` element contains three text
/// nodes: `"red-"`, `"or"`, and `"-blue"`. Each of these can produce multiple text chunks and each will
/// produce one text chunk where [`last_in_text_node`] returns `true`. The last chunk in a text
/// node can have empty textual content. To perform an action once on the text contents of an
/// element, see [`Element::end_tag_handlers`][crate::rewritable_units::Element::end_tag_handlers].
///
/// ## The text may contain entities
///
/// Text chunks are passed through as-is, without unescaping. The text may contain HTML entities, potentially
/// split across chunk boundaries. Text chunks may also appear inside `<style>` or `<script>`,
/// which do not support HTML entities.
///
/// It is important to observe [`TextChunk::text_type`] and unescape/escape the chunks accordingly.
///
/// ## The text may not be the bytes of the document
///
/// In a UTF-8 document, input that is not well-formed UTF-8 shows as U+FFFD in the text of a chunk.
/// The bytes themselves are kept: a chunk that no handler edits, replaces or removes is written
/// with the bytes it had in the input. Upstream lol-html writes the U+FFFD instead.
///
/// # Example
/// ```
/// use lol_html::{HtmlRewriter, Settings, text};
///
/// let mut greeting = String::new();
///
/// {
///     let mut rewriter = HtmlRewriter::new(
///         Settings {
///            element_content_handlers: vec![
///                text!("div", |t| {
///                  greeting += t.as_str();
///
///                  if t.last_in_text_node() {
///                         greeting += "!";
///                     }
///
///                     Ok(())
///                 })
///             ],
///             ..Settings::new()
///         },
///         |_:&[u8]| {}
///     );
///
///     rewriter.write(b"<div>He").unwrap();
///     rewriter.write(b"llo w").unwrap();
///     rewriter.write(b"orld</div>").unwrap();
///     rewriter.end().unwrap();
/// }
///
/// assert_eq!(greeting, "Hello world!");
/// ```
///
/// [`last_in_text_node`]: #method.last_in_text_node
pub struct TextChunk<'i> {
    text: Cow<'i, str>,
    text_type: TextType,
    last_in_text_node: bool,
    /// `true` once a handler may have edited the text in place.
    text_edited: bool,
    encoding: &'static Encoding,
    mutations: Mutations,
    user_data: Box<dyn Any + Send>,
    source_location: SourceLocation,
}

impl<'i> TextChunk<'i> {
    #[inline]
    #[must_use]
    pub(crate) fn new(
        text: &'i str,
        text_type: TextType,
        last_in_text_node: bool,
        encoding: &'static Encoding,
        source_location: SourceLocation,
    ) -> Self {
        TextChunk {
            text: text.into(),
            text_type,
            last_in_text_node,
            text_edited: false,
            encoding,
            mutations: Mutations::new(),
            user_data: Box::new(()),
            source_location,
        }
    }

    #[inline(always)]
    pub(crate) fn encoding(&self) -> &'static Encoding {
        self.encoding
    }

    /// Detaches the chunk from the parser's text buffer so it can outlive the
    /// current `write()` call (handler suspension). Leaves `self` behind in a
    /// drained state; the caller abandons it.
    pub(crate) fn take_owned(&mut self) -> TextChunk<'static> {
        TextChunk {
            text: Cow::Owned(std::mem::take(&mut self.text).into_owned()),
            text_type: self.text_type,
            last_in_text_node: self.last_in_text_node,
            text_edited: self.text_edited,
            encoding: self.encoding,
            mutations: std::mem::replace(&mut self.mutations, Mutations::new()),
            user_data: std::mem::replace(&mut self.user_data, Box::new(())),
            source_location: self.source_location.clone(),
        }
    }

    /// Returns the content of the chunk, which [`may not be a complete text node`](TextChunk).
    ///
    /// It may contain markup, such as HTML/XML entities. See [`TextChunk::text_type`].
    ///
    /// Because the text may be fragmented and contain markup, operating on the slice directly is tricky.
    /// It may be necessary to buffer the text. See [`TextChunk::last_in_text_node`].
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns the content of the chunk, which [`may not be a complete text node`](TextChunk).
    ///
    /// It may contain markup, such as HTML/XML entities. See [`TextChunk::text_type`].
    ///
    /// The string may be mutated, but needs to have markup escaped appropriately for the context its in.
    ///
    /// Because the text may be fragmented and contain markup, operating on the slice directly is tricky.
    /// It may be necessary to buffer the text. See [`TextChunk::last_in_text_node`].
    #[inline]
    pub fn as_mut_str(&mut self) -> &mut String {
        self.text_edited = true;
        self.text.to_mut()
    }

    /// Sets the content of this chunk only, without affecting text chunks around it.
    ///
    /// The markup must contain escaping appropriate for the context it's in, e.g. use `&amp` instead of `&` in HTML body.
    /// See [`TextChunk::text_type`].
    #[inline]
    pub fn set_str(&mut self, text: String) {
        self.text_edited = true;
        self.text = Cow::Owned(text);
    }

    /// Returns the type of the text in the chunk.
    ///
    /// The type of the text depends on the surrounding context of the text. E.g. regular visible
    /// text and text inside a `<script>` element will have different types. Refer to [`TextType`]
    /// for more information about possible text types.
    ///
    /// [`TextType`]: enum.TextType.html
    ///
    /// # Example
    ///
    /// ```
    /// use lol_html::{rewrite_str, text, RewriteStrSettings};
    /// use lol_html::html_content::TextType;
    ///
    /// let html = rewrite_str(
    ///     r#"<div>Hello</div><script>"use strict";</script>"#,
    ///     RewriteStrSettings {
    ///         element_content_handlers: vec![
    ///             text!("div", |t| {
    ///                 assert_eq!(t.text_type(), TextType::Data);
    ///
    ///                 Ok(())
    ///             }),
    ///             text!("script", |t| {
    ///                 assert_eq!(t.text_type(), TextType::ScriptData);
    ///
    ///                 Ok(())
    ///             })
    ///         ],
    ///         ..RewriteStrSettings::new()
    ///     }
    /// ).unwrap();
    /// ```
    #[inline]
    #[must_use]
    pub fn text_type(&self) -> TextType {
        self.text_type
    }

    /// Returns `true` if the chunk is last in a HTML text node.
    ///
    /// Note that last chunk can have empty textual content.
    ///
    /// In the current implementation, text nodes are interrupted by comments and CDATA.
    /// Use [`Element::end_tag_handlers`][crate::rewritable_units::Element::end_tag_handlers] to add content after all text in an element.
    #[inline]
    #[must_use]
    #[doc(alias = "is_last")]
    pub fn last_in_text_node(&self) -> bool {
        self.last_in_text_node
    }

    /// Inserts `content` before the text chunk.
    ///
    /// Consequent calls to the method append `content` to the previously inserted content.
    ///
    /// # Example
    ///
    /// ```
    /// use lol_html::{rewrite_str, text, RewriteStrSettings};
    /// use lol_html::html_content::ContentType;
    ///
    /// let html = rewrite_str(
    ///     r#"<div>world</div>"#,
    ///     RewriteStrSettings {
    ///         element_content_handlers: vec![
    ///             text!("div", |t| {
    ///                 if !t.last_in_text_node(){
    ///                     t.before("<!-- 42 -->", ContentType::Html);
    ///                     t.before("Hello ", ContentType::Text);
    ///                 }
    ///
    ///                 Ok(())
    ///             })
    ///         ],
    ///         ..RewriteStrSettings::new()
    ///     }
    /// ).unwrap();
    ///
    /// assert_eq!(html, r#"<div><!-- 42 -->Hello world</div>"#);
    /// ```
    #[inline]
    pub fn before(&mut self, content: &str, content_type: ContentType) {
        self.mutations
            .mutate()
            .content_before
            .push_back(StringChunk::from_str(content, content_type));
    }

    /// Inserts `content` after the text chunk.
    ///
    /// Consequent calls to the method prepend `content` to the previously inserted content.
    ///
    /// # Example
    ///
    /// ```
    /// use lol_html::{rewrite_str, text, RewriteStrSettings};
    /// use lol_html::html_content::ContentType;
    ///
    /// let html = rewrite_str(
    ///     r#"<div>Foo</div>"#,
    ///     RewriteStrSettings {
    ///         element_content_handlers: vec![
    ///             text!("div", |t| {
    ///                 if t.last_in_text_node(){
    ///                     t.after("Bar", ContentType::Text);
    ///                     t.after("Qux", ContentType::Text);
    ///                 }
    ///
    ///                 Ok(())
    ///             })
    ///         ],
    ///         ..RewriteStrSettings::new()
    ///     }
    /// ).unwrap();
    ///
    /// assert_eq!(html, r#"<div>FooQuxBar</div>"#);
    /// ```
    #[inline]
    pub fn after(&mut self, content: &str, content_type: ContentType) {
        self.mutations
            .mutate()
            .content_after
            .push_front(StringChunk::from_str(content, content_type));
    }

    /// Replaces the text chunk with the `content`.
    ///
    /// Consequent calls to the method overwrite previous replacement content.
    ///
    /// # Example
    ///
    /// ```
    /// use lol_html::{rewrite_str, text, RewriteStrSettings};
    /// use lol_html::html_content::ContentType;
    ///
    /// let html = rewrite_str(
    ///     r#"<div>Foo</div>"#,
    ///     RewriteStrSettings {
    ///         element_content_handlers: vec![
    ///             text!("div", |t| {
    ///                 if !t.last_in_text_node(){
    ///                     t.replace("Bar", ContentType::Text);
    ///                     t.replace("Qux", ContentType::Text);
    ///                 }
    ///
    ///                 Ok(())
    ///             })
    ///         ],
    ///         ..RewriteStrSettings::new()
    ///     }
    /// ).unwrap();
    ///
    /// assert_eq!(html, r#"<div>Qux</div>"#);
    /// ```
    #[inline]
    pub fn replace(&mut self, content: &str, content_type: ContentType) {
        self.mutations
            .mutate()
            .replace(StringChunk::from_str(content, content_type));
    }

    /// Inserts content from a [`StreamingHandler`] before the text chunk.
    ///
    /// Consequent calls to the method append `content` to the previously inserted content.
    ///
    /// Use the [`streaming!`] macro to make a `StreamingHandler` from a closure.
    pub fn streaming_before(&mut self, string_writer: Box<dyn StreamingHandler + Send + 'static>) {
        self.mutations
            .mutate()
            .content_before
            .push_back(StringChunk::stream(string_writer));
    }

    /// Inserts content from a [`StreamingHandler`] after the text chunk.
    ///
    /// Consequent calls to the method prepend to the previously inserted content.
    ///
    /// Use the [`streaming!`] macro to make a `StreamingHandler` from a closure.
    pub fn streaming_after(&mut self, string_writer: Box<dyn StreamingHandler + Send + 'static>) {
        self.mutations
            .mutate()
            .content_after
            .push_front(StringChunk::stream(string_writer));
    }

    /// Replaces the text chunk with the content from a [`StreamingHandler`].
    ///
    /// Consequent calls to the method overwrite previous replacement content.
    ///
    /// Use the [`streaming!`] macro to make a `StreamingHandler` from a closure.
    pub fn streaming_replace(&mut self, string_writer: Box<dyn StreamingHandler + Send + 'static>) {
        self.mutations
            .mutate()
            .replace(StringChunk::stream(string_writer));
    }

    /// Removes the text chunk.
    #[inline]
    pub fn remove(&mut self) {
        self.mutations.mutate().remove();
    }

    /// Returns `true` if the text chunk has been replaced or removed.
    #[inline]
    #[must_use]
    pub fn removed(&self) -> bool {
        self.mutations.removed()
    }

    #[inline]
    fn serialize_self(&self, sink: &mut StreamingHandlerSink<'_>) -> Result<(), RewritingError> {
        if !self.text.is_empty() {
            // The "text" here is actually markup
            sink.write_str(&self.text, ContentType::Html);
        }
        Ok(())
    }

    /// Writes a chunk whose text is a lossy decoding of `source` (malformed input
    /// shows in it as U+FFFD): `source` goes out in place of the text, unless a
    /// handler replaced or removed the chunk. Returns `false`, with nothing written,
    /// if a handler edited the text: the caller then serializes the chunk from its
    /// text, like any other token.
    ///
    /// Upstream lol-html writes the decoded text back here, and so does Cloudflare
    /// Workers: malformed input leaves as U+FFFD even when no handler touched the
    /// chunk. This fork keeps the source bytes instead, because a handler that only
    /// observes a chunk must not change the document.
    ///
    /// The content around the chunk follows the same order as `impl_serialize!`.
    ///
    /// The shape of this function keeps well-formed documents as fast as they were.
    /// It borrows the chunk, so `Token::into_bytes` stays the only consumer of one.
    /// It is `#[inline]` so that it is compiled with its callers and not into this
    /// crate: there it would bring the drop glue of the mutations with it, a crate
    /// built with `-Zshare-generics` (Bun) would link to that copy, and the drop of a
    /// chunk would no longer be inlined into `Token::into_bytes`. Either way costs
    /// about 20 instructions for every chunk of every document.
    #[cold]
    #[inline]
    pub(crate) fn write_from_source(
        &mut self,
        source: [&[u8]; 2],
        output_handler: &mut dyn FnMut(&[u8]),
    ) -> Result<bool, RewritingError> {
        if self.text_edited {
            return Ok(false);
        }
        let write_source = |output_handler: &mut dyn FnMut(&[u8])| {
            for part in source {
                if !part.is_empty() {
                    output_handler(part);
                }
            }
        };
        match self.mutations.take() {
            None => write_source(output_handler),
            Some(mutations) => {
                let mut sink = StreamingHandlerSink::new(self.encoding, output_handler);
                mutations
                    .content_before
                    .encode(&mut sink)
                    .map_err(RewritingError::ContentHandlerError)?;
                if !mutations.removed {
                    write_source(sink.output_handler());
                } else {
                    mutations
                        .replacement
                        .encode(&mut sink)
                        .map_err(RewritingError::ContentHandlerError)?;
                }
                mutations
                    .content_after
                    .encode(&mut sink)
                    .map_err(RewritingError::ContentHandlerError)?;
            }
        }
        Ok(true)
    }

    /// Position of this text fragment in the source document, before any rewriting
    #[must_use]
    pub fn source_location(&self) -> SourceLocation {
        self.source_location.clone()
    }
}

impl_serialize!(TextChunk);
impl_user_data!(TextChunk<'_>);

impl Debug for TextChunk<'_> {
    #[cold]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextChunk")
            .field("text", &self.as_str())
            .field("last_in_text_node", &self.last_in_text_node())
            .field("at", &self.source_location())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::html_content::*;
    use crate::rewritable_units::test_utils::*;
    use encoding_rs::{Encoding, UTF_8};

    fn rewrite_text_chunk(
        html: &[u8],
        encoding: &'static Encoding,
        mut handler: impl FnMut(&mut TextChunk<'_>),
    ) -> String {
        let mut handler_called = false;

        let output = rewrite_html(
            html,
            encoding,
            vec![],
            vec![doc_text!(|c| {
                handler_called = true;
                handler(c);
                Ok(())
            })],
        );

        assert!(handler_called);

        output
    }

    #[test]
    fn user_data() {
        rewrite_text_chunk(b"foo", UTF_8, |c| {
            c.set_user_data(42usize);

            assert_eq!(*c.user_data().downcast_ref::<usize>().unwrap(), 42usize);

            *c.user_data_mut().downcast_mut::<usize>().unwrap() = 1337usize;

            assert_eq!(*c.user_data().downcast_ref::<usize>().unwrap(), 1337usize);
        });
    }

    #[test]
    fn in_place_text_modifications() {
        let encoding = Encoding::for_label_no_replacement(b"utf-8").unwrap();
        let mut chunk = TextChunk::new(
            "original text",
            TextType::PlainText,
            true,
            encoding,
            SourceLocation::from_start_len(0, 0),
        );

        assert_eq!(chunk.as_str(), "original text");
        chunk.set_str("hello".to_owned());
        assert_eq!(chunk.as_str(), "hello");
        chunk.as_mut_str().push_str(" world!");
        assert_eq!(chunk.as_str(), "hello world!");
    }

    mod serialization {
        use super::*;

        const HTML: &str = "Lorem ipsum dolor sit amet, cÔnsectetur adipiscing elit, sed do eiusmod tempor \
             incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud \
             exercitation & ullamco laboris nisi ut aliquip ex ea commodo > consequat.";

        macro_rules! test {
            ($handler:expr, $expected:expr) => {
                for (html, enc) in encoded(HTML) {
                    assert_eq!(rewrite_text_chunk(&html, enc, $handler), $expected);
                }
            };
        }

        #[test]
        fn parsed() {
            test!(|_| {}, HTML);
        }

        #[test]
        fn with_prepends_and_appends() {
            let mut first = true;
            test!(
                |c| {
                    let is_first = std::mem::replace(&mut first, c.last_in_text_node());
                    if is_first {
                        c.before("<span>", ContentType::Text);
                        c.before("<div>Hey</div>", ContentType::Html);
                        c.before("<foo>", ContentType::Html);
                    }
                    if c.last_in_text_node() {
                        c.after("</foo>", ContentType::Html);
                        c.after("<!-- 42 -->", ContentType::Html);
                        c.after("<foo & bar>", ContentType::Text);
                    }
                },
                concat!(
                    "&lt;span&gt;<div>Hey</div><foo>",
                    "Lorem ipsum dolor sit amet, cÔnsectetur adipiscing elit, sed do eiusmod \
                     tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim \
                     veniam, quis nostrud exercitation & ullamco laboris nisi ut aliquip \
                     ex ea commodo > consequat.",
                    "&lt;foo &amp; bar&gt;<!-- 42 --></foo>"
                )
            );
        }

        #[test]
        fn removed() {
            let mut first = true;
            test!(
                |c| {
                    let is_first = std::mem::replace(&mut first, c.last_in_text_node());
                    assert!(!c.removed());

                    c.remove();

                    assert!(c.removed());

                    if is_first {
                        c.before("<before>", ContentType::Html);
                    }
                    if c.last_in_text_node() {
                        c.after("<after>", ContentType::Html);
                    }
                },
                "<before><after>"
            );
        }

        #[test]
        fn replaced_with_text() {
            test!(
                |c| {
                    if c.last_in_text_node() {
                        c.before("<before>", ContentType::Html);
                        c.after("<after>", ContentType::Html);

                        assert!(!c.removed());

                        c.replace("<div></div>", ContentType::Html);
                        c.replace("<!--42-->", ContentType::Html);
                        c.replace("<foo & bar>", ContentType::Text);

                        assert!(c.removed());
                    } else {
                        c.remove();
                    }
                },
                "<before>&lt;foo &amp; bar&gt;<after>"
            );
        }

        #[test]
        fn replaced_with_html() {
            test!(
                |c| {
                    if c.last_in_text_node() {
                        c.before("<before>", ContentType::Html);
                        c.after("<after>", ContentType::Html);

                        assert!(!c.removed());

                        c.replace("<div></div>", ContentType::Html);
                        c.replace("<!--42-->", ContentType::Html);
                        c.replace("<foo & bar>", ContentType::Html);

                        assert!(c.removed());
                    } else {
                        c.remove();
                    }
                },
                "<before><foo & bar><after>"
            );
        }

        #[test]
        fn last_flush_text_decoder() {
            let rewritten = rewrite_text_chunk(b"<p>\xF0\xF0\x9F\xF0\x9F\x98</p>", UTF_8, |c| {
                if c.last_in_text_node() {
                    c.after(" last", ContentType::Text);
                }
            });
            assert_eq!("<p>\u{fffd}\u{fffd}\u{fffd} last</p>", rewritten);
        }
    }

    /// What is written for a chunk of a UTF-8 document that no handler
    /// edited, replaced or removed: the bytes it was decoded from.
    mod source_bytes {
        use super::*;
        use crate::errors::{RewritingError, SuspensionRequest};
        use crate::{ElementContentHandlers, HandlerResult, HtmlRewriter, Selector, Settings};
        use std::borrow::Cow;
        use std::cell::RefCell;
        use std::rc::Rc;

        const SUSPEND: fn() -> HandlerResult = || Err(SuspensionRequest.into());

        /// A text handler for the elements `selector` matches.
        fn on_text<'h>(
            selector: &str,
            handler: impl FnMut(&mut TextChunk<'_>) -> HandlerResult + 'h,
        ) -> (Cow<'static, Selector>, ElementContentHandlers<'h>) {
            (
                Cow::Owned(selector.parse().unwrap()),
                ElementContentHandlers::default().text(handler),
            )
        }

        /// Documents with text that is not well-formed UTF-8.
        const DAMAGED: &[&[u8]] = &[
            // windows-1252 and Shift_JIS text
            b"<p>caf\xE9</p>",
            b"<p>\x93\xFA\x96\x7B</p>",
            // an overlong sequence, a byte that is never valid, a surrogate
            b"<p>a\xC0\xAF\xFF\xED\xA0\x80b</p>",
            // sequences cut short by the end tag, by a comment, by the end of the input
            b"<p>aa\xE2</p>",
            b"<p>aa\xE2\x82</p>",
            b"<p>\xF0\x9F\x98</p>",
            b"<p>\xE2</p>",
            b"<p>caf\xC3<!---->\xA9</p>",
            b"caf\xE9",
            b"<p>caf\xE2\x82",
            // every kind of text
            b"<script>var x = '\xE9';</script>",
            b"<style>a:after{content:'\xE9'}</style>",
            b"<title>caf\xE9</title>",
            b"<textarea>caf\xE9</textarea>",
            b"<svg><![CDATA[caf\xE9]]></svg>",
            b"<plaintext>caf\xE9",
            // several text nodes, and text longer than the decoding buffer
            b"<p>caf\xE9<b>\xFF</b>\xC0</p>tail \xE9",
            b"<p>0123456789abcdef\xE9 0123456789abcdef\xE9 0123456789abcdef</p>",
            b"<p>\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9\xE9</p>",
        ];

        /// `document` as one write, as one write per byte, and in between.
        fn write_patterns(document: &[u8]) -> Vec<Vec<&[u8]>> {
            [document.len().max(1), 1, 2, 3, 5]
                .into_iter()
                .map(|size| document.chunks(size).collect())
                .collect()
        }

        type Chunks = Rc<RefCell<Vec<(String, bool)>>>;

        /// A handler that records the chunks it is given and changes nothing.
        fn observe(
            chunks: &Chunks,
            suspend: bool,
        ) -> impl FnMut(&mut TextChunk<'_>) -> HandlerResult + use<> {
            let chunks = Rc::clone(chunks);
            move |t| {
                chunks
                    .borrow_mut()
                    .push((t.as_str().to_owned(), t.last_in_text_node()));
                if suspend { SUSPEND() } else { Ok(()) }
            }
        }

        #[test]
        fn a_handler_that_changes_nothing_keeps_the_bytes() {
            for &document in DAMAGED {
                for writes in write_patterns(document) {
                    for suspend in [false, true] {
                        let doc_chunks = Chunks::default();
                        let element_chunks = Chunks::default();
                        let (output, suspensions) = rewrite_writes(
                            &writes,
                            Settings {
                                element_content_handlers: vec![on_text(
                                    "*",
                                    observe(&element_chunks, suspend),
                                )],
                                document_content_handlers: vec![doc_text!(observe(
                                    &doc_chunks,
                                    suspend
                                ))],
                                ..Settings::new()
                            },
                            |_| {},
                        );

                        assert_eq!(
                            output, document,
                            "{document:x?} written as {writes:x?}, suspend: {suspend}"
                        );
                        // The handlers still get the decoded text.
                        let seen: String = doc_chunks.borrow().iter().map(|c| &*c.0).collect();
                        assert!(seen.contains('\u{fffd}'), "{seen:?}");
                        assert_eq!(suspend, suspensions != 0);
                    }
                }
            }
        }

        #[test]
        fn handlers_get_the_decoded_text() {
            let chunks_of = |document: &[u8]| {
                let chunks = Chunks::default();
                rewrite_writes(
                    &[document],
                    Settings {
                        document_content_handlers: vec![doc_text!(observe(&chunks, false))],
                        ..Settings::new()
                    },
                    |_| {},
                );
                chunks.take()
            };

            assert_eq!(
                chunks_of(b"<p>a\xA9b</p>"),
                [("a\u{fffd}b".to_owned(), false), (String::new(), true)]
            );
            // The bytes of a sequence that the end of the text node cuts short
            // belong to the chunk that shows their U+FFFD, the last one.
            assert_eq!(chunks_of(b"<p>\xE2</p>"), [("\u{fffd}".to_owned(), true)]);
            assert_eq!(
                chunks_of(b"<p>aa\xE2\x82</p>"),
                [("aa".to_owned(), false), ("\u{fffd}".to_owned(), true)]
            );
        }

        #[test]
        fn content_around_the_chunk_keeps_the_bytes() {
            for suspend in [false, true] {
                let (output, _) = rewrite_writes(
                    &[b"<p>caf\xE9!</p><p>th\xE9</p>"],
                    Settings {
                        document_content_handlers: vec![doc_text!(move |t| {
                            if !t.as_str().is_empty() {
                                t.before("<b>[", ContentType::Html);
                                t.after("]</b>", ContentType::Html);
                            }
                            if suspend { SUSPEND() } else { Ok(()) }
                        })],
                        ..Settings::new()
                    },
                    |_| {},
                );
                // `th\xE9` is two chunks: 0xE9 could start a character, so its
                // U+FFFD comes with the chunk that ends the text node.
                assert_eq!(
                    output,
                    b"<p><b>[caf\xE9!]</b></p><p><b>[th]</b><b>[\xE9]</b></p>"
                );
            }
        }

        #[test]
        fn a_replaced_or_removed_chunk_drops_the_bytes() {
            let rewrite = |handler: fn(&mut TextChunk<'_>)| {
                rewrite_writes(
                    &[b"<p>caf\xE9!</p>"],
                    Settings {
                        document_content_handlers: vec![doc_text!(move |t| {
                            handler(t);
                            Ok(())
                        })],
                        ..Settings::new()
                    },
                    |_| {},
                )
                .0
            };

            assert_eq!(rewrite(|t| t.replace("X", ContentType::Text)), b"<p>XX</p>");
            assert_eq!(rewrite(|t| t.remove()), b"<p></p>");
            assert_eq!(
                rewrite(|t| {
                    t.before("[", ContentType::Text);
                    t.remove();
                    t.after("]", ContentType::Text);
                }),
                b"<p>[][]</p>"
            );
        }

        #[test]
        fn an_edited_chunk_is_written_as_edited() {
            let rewrite = |handler: fn(&mut TextChunk<'_>)| {
                rewrite_writes(
                    &[b"<p>caf\xE9!</p>"],
                    Settings {
                        document_content_handlers: vec![doc_text!(move |t| {
                            if !t.last_in_text_node() {
                                handler(t);
                            }
                            Ok(())
                        })],
                        ..Settings::new()
                    },
                    |_| {},
                )
                .0
            };

            assert_eq!(
                rewrite(|t| t.as_mut_str().push('?')),
                "<p>caf\u{fffd}!?</p>".as_bytes()
            );
            assert_eq!(
                rewrite(|t| t.set_str("tea".to_owned())),
                "<p>tea</p>".as_bytes()
            );
            // Taking the text to edit it counts as an edit.
            assert_eq!(
                rewrite(|t| {
                    let _ = t.as_mut_str();
                }),
                "<p>caf\u{fffd}!</p>".as_bytes()
            );
        }

        #[test]
        fn a_parked_chunk_keeps_the_bytes_until_it_is_changed() {
            let rewrite = |on_suspend: fn(&mut TextChunk<'static>)| {
                rewrite_writes(
                    &[b"<p>caf\xE9!</p>"],
                    Settings {
                        document_content_handlers: vec![doc_text!(|t| {
                            if t.last_in_text_node() {
                                Ok(())
                            } else {
                                SUSPEND()
                            }
                        })],
                        ..Settings::new()
                    },
                    |rewriter| on_suspend(rewriter.suspended_text_chunk().unwrap()),
                )
            };

            assert_eq!(rewrite(|_| {}), (b"<p>caf\xE9!</p>".to_vec(), 1));
            assert_eq!(
                rewrite(|t| t.before("[", ContentType::Text)).0,
                b"<p>[caf\xE9!</p>"
            );
            assert_eq!(
                rewrite(|t| t.replace("tea", ContentType::Text)).0,
                b"<p>tea</p>"
            );
            assert_eq!(
                rewrite(|t| t.as_mut_str().push('?')).0,
                "<p>caf\u{fffd}!?</p>".as_bytes()
            );
        }

        #[test]
        fn a_chunk_edited_before_it_is_parked_is_written_as_edited() {
            let (output, suspensions) = rewrite_writes(
                &[b"<p>caf\xE9!</p>"],
                Settings {
                    document_content_handlers: vec![doc_text!(|t| {
                        if !t.last_in_text_node() {
                            t.as_mut_str().push('?');
                        }
                        SUSPEND()
                    })],
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, "<p>caf\u{fffd}!?</p>".as_bytes());
            assert_eq!(suspensions, 2);
        }

        #[test]
        fn a_chunk_parked_twice_keeps_the_bytes() {
            let (output, suspensions) = rewrite_writes(
                &[b"<p>caf\xE9!</p><p>th\xE9</p>"],
                Settings {
                    document_content_handlers: vec![
                        doc_text!(|_| SUSPEND()),
                        doc_text!(|_| SUSPEND()),
                    ],
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, b"<p>caf\xE9!</p><p>th\xE9</p>");
            assert_eq!(suspensions, 8);
        }

        #[test]
        fn bytes_of_a_chunk_that_is_not_written_do_not_reach_a_later_chunk() {
            // The text of the removed element is parked with its source bytes.
            let (output, suspensions) = rewrite_writes(
                &[b"<div>caf\xE9!</div><p>tea</p>"],
                Settings {
                    element_content_handlers: vec![element!("div", |el| {
                        el.remove();
                        Ok(())
                    })],
                    document_content_handlers: vec![doc_text!(|_| SUSPEND())],
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, b"<p>tea</p>");
            assert_eq!(suspensions, 4);
        }

        #[test]
        fn a_later_handler_can_change_a_chunk_after_a_suspension() {
            let (output, suspensions) = rewrite_writes(
                &[b"<p>caf\xE9!</p><p>th\xE9!</p>"],
                Settings {
                    document_content_handlers: vec![
                        doc_text!(|_| SUSPEND()),
                        doc_text!(|t| {
                            if t.as_str().starts_with("th") {
                                t.replace("tea", ContentType::Text);
                            }
                            Ok(())
                        }),
                    ],
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, b"<p>caf\xE9!</p><p>tea</p>");
            assert_eq!(suspensions, 4);
        }

        #[test]
        fn bytes_stay_with_the_chunk_that_shows_their_character() {
            // U+1F600 and U+20AC split by a write. Whichever chunk a handler
            // replaces, no part of a character is left behind or lost.
            let split_in_well_formed_text: &[&[u8]] = &[b"<p>a\xF0\x9F", b"\x98\x80b</p>"];
            let split_after_damage: &[&[u8]] = &[b"<p>caf\xE9\xE2", b"\x82\xACb</p>"];

            let rewrite = |writes: &[&[u8]], replaced: &'static str, suspend: bool| {
                rewrite_writes(
                    writes,
                    Settings {
                        element_content_handlers: vec![on_text("p", move |t| {
                            if !replaced.is_empty() && t.as_str().contains(replaced) {
                                t.replace("X", ContentType::Text);
                            }
                            if suspend { SUSPEND() } else { Ok(()) }
                        })],
                        ..Settings::new()
                    },
                    |_| {},
                )
                .0
            };

            for suspend in [false, true] {
                assert_eq!(
                    rewrite(split_in_well_formed_text, "", suspend),
                    "<p>a\u{1F600}b</p>".as_bytes()
                );
                assert_eq!(
                    rewrite(split_in_well_formed_text, "\u{1F600}", suspend),
                    b"<p>aX</p>"
                );
                assert_eq!(
                    rewrite(split_in_well_formed_text, "a", suspend),
                    "<p>X\u{1F600}b</p>".as_bytes()
                );

                assert_eq!(
                    rewrite(split_after_damage, "", suspend),
                    b"<p>caf\xE9\xE2\x82\xACb</p>"
                );
                assert_eq!(
                    rewrite(split_after_damage, "\u{20ac}", suspend),
                    b"<p>caf\xE9X</p>"
                );
                assert_eq!(
                    rewrite(split_after_damage, "caf", suspend),
                    "<p>X\u{20ac}b</p>".as_bytes()
                );
            }
        }

        #[test]
        fn a_write_that_only_continues_a_character_makes_no_chunk() {
            let chunks = Chunks::default();
            let mut observe = observe(&chunks, false);
            let (output, _) = rewrite_writes(
                &[b"<p>a", b"\xE2\x82", b"\xACb</p>"],
                Settings {
                    element_content_handlers: vec![on_text("p", move |t| {
                        t.before("|", ContentType::Text);
                        observe(t)
                    })],
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, "<p>|a|\u{20ac}b|</p>".as_bytes());
            assert_eq!(
                *chunks.borrow(),
                [
                    ("a".to_owned(), false),
                    ("\u{20ac}b".to_owned(), false),
                    (String::new(), true)
                ]
            );
        }

        #[test]
        fn content_after_the_last_chunk_follows_the_bytes() {
            // `last_flush_text_decoder`, byte for byte.
            let (output, _) = rewrite_writes(
                &[b"<p>\xF0\xF0\x9F\xF0\x9F\x98</p>"],
                Settings {
                    document_content_handlers: vec![doc_text!(|t| {
                        if t.last_in_text_node() {
                            t.after(" last", ContentType::Text);
                        }
                        Ok(())
                    })],
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, b"<p>\xF0\xF0\x9F\xF0\x9F\x98 last</p>");
        }

        #[test]
        fn content_around_a_lossy_chunk_is_ordered_as_around_any_chunk() {
            // Every way to put content around a chunk, to replace it and to remove it.
            let scripts: [fn(&mut TextChunk<'_>); 6] = [
                |_| {},
                |t| {
                    t.before("<1>", ContentType::Html);
                    t.before("<2>", ContentType::Text);
                    t.after("<3>", ContentType::Html);
                    t.after("<4>", ContentType::Text);
                },
                |t| {
                    t.after("<1>", ContentType::Html);
                    t.replace("<2>", ContentType::Html);
                    t.replace("<3>", ContentType::Text);
                    t.before("<4>", ContentType::Html);
                },
                |t| {
                    t.before("<1>", ContentType::Html);
                    t.remove();
                    t.after("<2>", ContentType::Html);
                },
                |t| {
                    t.streaming_before(streaming!(|s| {
                        s.write_str("<1>", ContentType::Html);
                        Ok(())
                    }));
                    t.before("<2>", ContentType::Html);
                    t.streaming_after(streaming!(|s| {
                        s.write_str("<3>", ContentType::Text);
                        Ok(())
                    }));
                    t.after("<4>", ContentType::Html);
                },
                |t| {
                    t.before("<1>", ContentType::Html);
                    t.streaming_replace(streaming!(|s| {
                        s.write_str("<2>", ContentType::Html);
                        Ok(())
                    }));
                    t.after("<3>", ContentType::Html);
                },
            ];

            for (n, script) in scripts.into_iter().enumerate() {
                for suspend in [false, true] {
                    let rewrite = |document: &[u8]| {
                        rewrite_writes(
                            &[document],
                            Settings {
                                document_content_handlers: vec![doc_text!(move |t| {
                                    if !t.as_str().is_empty() {
                                        script(t);
                                    }
                                    if suspend { SUSPEND() } else { Ok(()) }
                                })],
                                ..Settings::new()
                            },
                            |_| {},
                        )
                        .0
                    };

                    // The same document with well-formed text: its chunk is
                    // written by `impl_serialize!`.
                    let twin = rewrite(b"<p>caf@!</p>");
                    let expected = match twin.windows(5).position(|w| w == b"caf@!") {
                        Some(at) => [&twin[..at], b"caf\xE9!", &twin[at + 5..]].concat(),
                        None => twin,
                    };
                    assert_eq!(
                        rewrite(b"<p>caf\xE9!</p>"),
                        expected,
                        "script {n}, suspend: {suspend}"
                    );
                }
            }
        }

        #[test]
        fn an_error_from_content_around_a_lossy_chunk_fails_the_rewrite() {
            let scripts: [fn(&mut TextChunk<'_>); 3] = [
                |t| t.streaming_before(streaming!(|_| Err("no content".into()))),
                |t| t.streaming_replace(streaming!(|_| Err("no content".into()))),
                |t| t.streaming_after(streaming!(|_| Err("no content".into()))),
            ];

            for (n, script) in scripts.into_iter().enumerate() {
                // A lossy chunk, and a well-formed one for comparison.
                for document in [&b"<p>caf\xE9!</p>"[..], b"<p>caf@!</p>"] {
                    let mut rewriter = HtmlRewriter::new(
                        Settings {
                            document_content_handlers: vec![doc_text!(move |t| {
                                // Not the empty chunk that ends the text node.
                                if !t.as_str().is_empty() {
                                    script(t);
                                }
                                Ok(())
                            })],
                            ..Settings::new()
                        },
                        |_: &[u8]| {},
                    );
                    let res = rewriter.write(document);
                    assert!(
                        matches!(res, Err(RewritingError::ContentHandlerError(_))),
                        "script {n}, {document:x?}: {res:?}"
                    );
                }
            }
        }

        #[test]
        fn a_document_in_another_encoding_is_written_as_before() {
            use crate::AsciiCompatibleEncoding;
            use encoding_rs::WINDOWS_1252;

            // Shift_JIS cannot decode 0xFF: the chunk has a U+FFFD, and what is
            // written is that character as Shift_JIS can express it.
            let (output, _) = rewrite_writes(
                &[b"<p>a\xFFb \x93\xFA</p>"],
                Settings {
                    document_content_handlers: vec![doc_text!(|_| Ok(()))],
                    encoding: AsciiCompatibleEncoding::new(encoding_rs::SHIFT_JIS).unwrap(),
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, b"<p>a&#65533;b \x93\xFA</p>");

            // Every byte is a character in windows-1252.
            let (output, _) = rewrite_writes(
                &[b"<p>caf\xE9 \x81</p>"],
                Settings {
                    document_content_handlers: vec![doc_text!(|_| Ok(()))],
                    encoding: AsciiCompatibleEncoding::new(WINDOWS_1252).unwrap(),
                    ..Settings::new()
                },
                |_| {},
            );
            assert_eq!(output, b"<p>caf\xE9 \x81</p>");
        }

        /// Text nodes of every kind, with tags, comments and nothing between them.
        fn random_document(rng: &mut Rng, damage: bool) -> Vec<u8> {
            let mut document = vec![];
            for _ in 0..1 + rng.below(5) {
                let (open, close): (&[u8], &[u8]) = match rng.below(9) {
                    0 => (b"<script>", b"</script>"),
                    1 => (b"<title>", b"</title>"),
                    2 => (b"<style>", b"</style>"),
                    3 => (b"<div><!--c-->", b"</div>"),
                    4 => (b"<div>", b"<b>x</b></div>"),
                    5 => (b"", b""),
                    _ => (b"<p>", b"</p>"),
                };
                document.extend_from_slice(open);
                let len = match rng.below(6) {
                    0 => 0,
                    1 => 1 + rng.below(4),
                    2 => 30 + rng.below(60),
                    _ => 5 + rng.below(25),
                };
                random_text(rng, len as usize, damage, &mut document);
                document.extend_from_slice(close);
            }
            document
        }

        #[test]
        fn handlers_that_change_nothing_never_change_the_output() {
            // LOL_HTML_FUZZ_DOCUMENTS=200000 for a longer run.
            let documents = std::env::var("LOL_HTML_FUZZ_DOCUMENTS")
                .ok()
                .map_or(1_500, |n| n.parse::<u64>().unwrap());
            let mut lossy_documents = 0;
            let mut total_suspensions = 0;

            for seed in 1..=documents {
                let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
                let damage = seed % 4 != 0;
                let document = random_document(&mut rng, damage);
                let cuts = random_cuts(&mut rng, document.len());
                let mut writes = vec![];
                let mut start = 0;
                for &end in &cuts {
                    writes.push(&document[start..end]);
                    start = end;
                }
                lossy_documents += u64::from(std::str::from_utf8(&document).is_err());

                // 0: one handler; 1: two handlers that suspend at random.
                for mode in 0..2 {
                    let suspend_rng = Rc::new(RefCell::new(Rng(seed | 1)));
                    let handler = |suspend_rng: &Rc<RefCell<Rng>>| {
                        let suspend_rng = Rc::clone(suspend_rng);
                        move |_: &mut TextChunk<'_>| {
                            if mode == 1 && suspend_rng.borrow_mut().below(3) == 0 {
                                SUSPEND()
                            } else {
                                Ok(())
                            }
                        }
                    };
                    let mut settings = Settings {
                        document_content_handlers: vec![doc_text!(handler(&suspend_rng))],
                        ..Settings::new()
                    };
                    if mode == 1 {
                        settings
                            .element_content_handlers
                            .push(on_text("*", handler(&suspend_rng)));
                    }

                    let (output, suspensions) = rewrite_writes(&writes, settings, |_| {});
                    assert_eq!(
                        output, document,
                        "seed {seed}, mode {mode}: {document:x?} cut at {cuts:?}"
                    );
                    total_suspensions += suspensions;
                }
            }

            // The documents do reach the paths this test is about.
            assert!(lossy_documents > documents / 2, "{lossy_documents}");
            assert!(total_suspensions > 0);
        }

        /// Handlers that change elements and remove every comment.
        fn other_handlers<'h>(element_change: u64) -> Settings<'h, 'static> {
            Settings {
                element_content_handlers: vec![element!("div, b, p", move |el| {
                    match element_change {
                        0 => el.remove_and_keep_content(),
                        1 => el.remove(),
                        2 => {
                            el.before("<i>", ContentType::Html);
                            el.after("</i>", ContentType::Html);
                        }
                        3 => {
                            el.prepend("[", ContentType::Text);
                            el.append("]", ContentType::Text);
                        }
                        4 => el.set_inner_content("x", ContentType::Text),
                        _ => el.replace("<hr>", ContentType::Html),
                    }
                    Ok(())
                })],
                document_content_handlers: vec![doc_comments!(|c| {
                    c.remove();
                    Ok(())
                })],
                ..Settings::new()
            }
        }

        /// `other_handlers`, and text handlers that change nothing.
        fn other_handlers_and_text<'h>(
            element_change: u64,
            chunks: &Chunks,
            suspend: bool,
        ) -> Settings<'h, 'static> {
            let mut settings = other_handlers(element_change);
            settings
                .element_content_handlers
                .push(on_text("*", observe(&Chunks::default(), suspend)));
            settings
                .document_content_handlers
                .push(doc_text!(observe(chunks, suspend)));
            settings
        }

        #[test]
        fn bytes_that_a_removed_token_separated_are_written_side_by_side() {
            // The tags and the comment go, and the bytes around them make a
            // U+20AC that no text handler was given. The output is the same
            // without a text handler.
            for document in [
                &b"<p>\xE2<b>\x82</b>\xAC</p>"[..],
                b"<p>\xE2<!--c-->\x82\xAC</p>",
            ] {
                let (without_text_handlers, _) =
                    rewrite_writes(&[document], other_handlers(0), |_| {});
                assert_eq!(without_text_handlers, "\u{20ac}".as_bytes());

                for suspend in [false, true] {
                    let chunks = Chunks::default();
                    let (output, _) = rewrite_writes(
                        &[document],
                        other_handlers_and_text(0, &chunks, suspend),
                        |_| {},
                    );
                    assert_eq!(output, without_text_handlers);
                    let seen: String = chunks.borrow().iter().map(|c| &*c.0).collect();
                    assert!(seen.chars().all(|c| c == '\u{fffd}'), "{seen:?}");
                }
            }
        }

        #[test]
        fn handlers_that_change_nothing_change_no_byte_whatever_other_handlers_do() {
            let mut changed_documents = 0;

            for seed in 1..=400u64 {
                let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
                let document = random_document(&mut rng, true);
                let cuts = random_cuts(&mut rng, document.len());
                let mut writes = vec![];
                let mut start = 0;
                for &end in &cuts {
                    writes.push(&document[start..end]);
                    start = end;
                }

                for element_change in 0..6 {
                    let (without_text_handlers, _) =
                        rewrite_writes(&writes, other_handlers(element_change), |_| {});
                    changed_documents += u64::from(without_text_handlers != document);

                    for suspend in [false, true] {
                        let (output, _) = rewrite_writes(
                            &writes,
                            other_handlers_and_text(element_change, &Chunks::default(), suspend),
                            |_| {},
                        );
                        assert_eq!(
                            output, without_text_handlers,
                            "seed {seed}, element change {element_change}, suspend: {suspend}: \
                             {document:x?} cut at {cuts:?}"
                        );
                    }
                }
            }

            // The other handlers do change the documents.
            assert!(changed_documents > 1_000, "{changed_documents}");
        }

        #[cfg(target_pointer_width = "64")]
        #[test]
        fn a_chunk_is_no_larger_for_it() {
            assert_eq!(size_of::<TextChunk<'_>>(), 80);
        }
    }
}
