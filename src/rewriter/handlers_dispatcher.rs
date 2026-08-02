use super::ElementDescriptor;
use super::is_suspension_request;
use super::settings::*;
use crate::rewritable_units::{
    Comment, Doctype, DocumentEnd, Element, EndTag, StartTag, TextChunk, Token, TokenCaptureFlags,
};
use crate::selectors_vm::{MatchId, MatchInfo};
use std::num::NonZero;

/// A rewritable unit detached from the parser's stack by a handler
/// suspension. Boxed so its address stays stable across `resume` calls: an
/// embedder may hold a raw pointer to it for the whole suspension.
pub(crate) enum SuspendedToken<H: HandlerTypes> {
    Element {
        element: Box<Element<'static, 'static, H>>,
        /// Index of the first element handler not yet run for this token.
        next_handler_idx: usize,
    },
    EndTag(Box<EndTag<'static>>),
    TextChunk {
        chunk: Box<TextChunk<'static>>,
        next_handler_idx: usize,
    },
    Comment {
        comment: Box<Comment<'static>>,
        next_handler_idx: usize,
    },
    Doctype {
        doctype: Box<Doctype<'static>>,
        next_handler_idx: usize,
    },
    DocumentEnd(Box<DocumentEnd<'static>>),
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SelectorHandlersLocator {
    pub element_handler_idx: Option<Locator>,
    pub comment_handler_idx: Option<Locator>,
    pub text_handler_idx: Option<Locator>,
}

/// It's index+1, allows efficient Option<Locator> (unfortunately Rust has no non-FFFF type)
pub(crate) type Locator = NonZero<u32>;

fn locator_to_idx(locator: Locator) -> usize {
    (locator.get() - 1) as usize
}

struct HandlerVecItem<H> {
    handler: H,
    user_count: u32,
}

struct HandlerVec<H> {
    items: Vec<HandlerVecItem<H>>,
    user_count: u32,
}

impl<H> Default for HandlerVec<H> {
    fn default() -> Self {
        Self {
            items: Vec::default(),
            user_count: 0,
        }
    }
}

impl<H> HandlerVec<H> {
    #[inline]
    pub fn push(&mut self, handler: H, always_active: bool) -> Option<Locator> {
        let item = HandlerVecItem {
            handler,
            user_count: u32::from(always_active),
        };

        self.user_count += item.user_count;
        self.items.push(item);

        let locator = self.items.len().try_into().ok().and_then(NonZero::new);
        debug_assert!(locator.is_some());
        locator
    }

    #[inline]
    pub fn inc_user_count(&mut self, idx: Locator) {
        let Some(item) = self.items.get_mut(locator_to_idx(idx)) else {
            debug_assert!(false);
            return;
        };
        item.user_count += 1;
        self.user_count += 1;
    }

    #[inline]
    pub fn dec_user_count(&mut self, idx: Locator) {
        let Some(item) = self.items.get_mut(locator_to_idx(idx)) else {
            debug_assert!(false);
            return;
        };
        debug_assert!(item.user_count > 0);
        debug_assert!(self.user_count > 0);
        item.user_count -= 1;
        self.user_count -= 1;
    }

    #[inline]
    pub const fn has_active(&self) -> bool {
        self.user_count > 0
    }

    /// Runs `cb` for every active item starting from `start_idx`. `*next_idx`
    /// is kept pointed past the item currently being invoked, so a handler
    /// that suspends (returns `Err`) can be resumed from the following item.
    #[inline]
    pub fn for_each_active_from(
        &mut self,
        start_idx: usize,
        next_idx: &mut usize,
        mut cb: impl FnMut(&mut H) -> HandlerResult,
    ) -> HandlerResult {
        for i in start_idx..self.items.len() {
            if self.items[i].user_count > 0 {
                *next_idx = i + 1;
                cb(&mut self.items[i].handler)?;
            }
        }

        *next_idx = self.items.len();
        Ok(())
    }

    /// Like [`Self::for_each_active_from`], but each visited item is
    /// deactivated. Deactivation happens *before* an error is propagated, so
    /// a handler that suspends is not re-run when the loop is resumed from
    /// `*next_idx`.
    #[inline]
    pub fn do_for_each_active_and_deactivate_from(
        &mut self,
        start_idx: usize,
        next_idx: &mut usize,
        mut cb: impl FnMut(&mut H) -> HandlerResult,
    ) -> HandlerResult {
        for i in start_idx..self.items.len() {
            if self.items[i].user_count > 0 {
                *next_idx = i + 1;

                let item = &mut self.items[i];
                let res = cb(&mut item.handler);

                self.user_count -= item.user_count;
                item.user_count = 0;

                res?;
            }
        }

        *next_idx = self.items.len();
        Ok(())
    }

    pub fn do_for_each_active_and_remove_tail(
        &mut self,
        mut cb: impl FnMut(H) -> HandlerResult,
    ) -> HandlerResult {
        // already-handled end tag handlers may be first, and they must not be removed
        if let Some(first) = self.items.iter().position(|item| item.user_count > 0) {
            // Must drop everything after, as remove() would change indexes anyway, breaking locators.
            // Popping from the back is for backwards-compat with previous implementation.
            //
            // Items are popped one at a time rather than drained: a handler that suspends
            // returns `Err`, and the handlers that have not run yet must stay in place (with
            // `user_count` intact) so the resumed call runs them.
            while self.items.len() > first {
                let Some(item) = self.items.pop() else {
                    break;
                };
                if item.user_count > 0 {
                    self.user_count -= item.user_count;
                    cb(item.handler)?;
                }
            }
        }
        debug_assert_eq!(self.user_count, 0);
        Ok(())
    }
}

pub(crate) struct ContentHandlersDispatcher<'h, H: HandlerTypes> {
    doctype_handlers: HandlerVec<H::DoctypeHandler<'h>>,
    comment_handlers: HandlerVec<H::CommentHandler<'h>>,
    text_handlers: HandlerVec<H::TextHandler<'h>>,
    end_tag_handlers: HandlerVec<H::EndTagHandler<'static>>,
    element_handlers: HandlerVec<H::ElementHandler<'h>>,
    end_handlers: HandlerVec<H::EndHandler<'h>>,
    next_element_can_have_content: bool,
    matched_elements_with_removed_content: usize,
    /// Dense index by match_id
    locators: Vec<SelectorHandlersLocator>,
    /// The rewritable unit a handler suspended on, if any. See
    /// [`crate::SuspensionRequest`].
    suspended: Option<SuspendedToken<H>>,
}

impl<H: HandlerTypes> Default for ContentHandlersDispatcher<'_, H> {
    fn default() -> Self {
        ContentHandlersDispatcher {
            doctype_handlers: Default::default(),
            comment_handlers: Default::default(),
            text_handlers: Default::default(),
            end_tag_handlers: Default::default(),
            element_handlers: Default::default(),
            end_handlers: Default::default(),
            next_element_can_have_content: false,
            matched_elements_with_removed_content: 0,
            locators: Vec::new(),
            suspended: None,
        }
    }
}

impl<'h, H: HandlerTypes> ContentHandlersDispatcher<'h, H> {
    #[inline]
    pub fn add_document_content_handlers(&mut self, handlers: DocumentContentHandlers<'h, H>) {
        if let Some(handler) = handlers.doctype {
            self.doctype_handlers.push(handler, true);
        }

        if let Some(handler) = handlers.comments {
            self.comment_handlers.push(handler, true);
        }

        if let Some(handler) = handlers.text {
            self.text_handlers.push(handler, true);
        }

        if let Some(handler) = handlers.end {
            self.end_handlers.push(handler, true);
        }
    }

    #[inline]
    pub fn add_selector_associated_handlers(
        &mut self,
        handlers: ElementContentHandlers<'h, H>,
    ) -> MatchId {
        let match_id = self.locators.len() as MatchId;
        self.locators.push(SelectorHandlersLocator {
            element_handler_idx: handlers
                .element
                .and_then(|h| self.element_handlers.push(h, false)),
            comment_handler_idx: handlers
                .comments
                .and_then(|h| self.comment_handlers.push(h, false)),
            text_handler_idx: handlers
                .text
                .and_then(|h| self.text_handlers.push(h, false)),
        });
        match_id
    }

    #[inline]
    pub const fn has_matched_elements_with_removed_content(&self) -> bool {
        self.matched_elements_with_removed_content > 0
    }

    #[inline]
    pub fn start_matching(&mut self, match_info: &MatchInfo) {
        let Some(locator) = self.locators.get(match_info.match_id as usize) else {
            debug_assert!(false);
            return;
        };

        if match_info.with_content {
            if let Some(idx) = locator.comment_handler_idx {
                self.comment_handlers.inc_user_count(idx);
            }

            if let Some(idx) = locator.text_handler_idx {
                self.text_handlers.inc_user_count(idx);
            }
        }

        if let Some(idx) = locator.element_handler_idx {
            self.element_handlers.inc_user_count(idx);
        }

        self.next_element_can_have_content = match_info.with_content;
    }

    #[inline]
    pub fn stop_matching(&mut self, elem_desc: ElementDescriptor) {
        for match_id in elem_desc.matched_content_handlers.iter() {
            let Some(locator) = self.locators.get(match_id as usize) else {
                debug_assert!(false);
                continue;
            };

            if let Some(idx) = locator.comment_handler_idx {
                self.comment_handlers.dec_user_count(idx);
            }

            if let Some(idx) = locator.text_handler_idx {
                self.text_handlers.dec_user_count(idx);
            }
        }

        if let Some(idx) = elem_desc.end_tag_handler_idx {
            self.end_tag_handlers.inc_user_count(idx);
        }

        if elem_desc.remove_content {
            self.matched_elements_with_removed_content -= 1;
        }
    }

    /// The post-handler bookkeeping for an element: mark its descriptor on
    /// the open-element stack with the `remove()`/end-tag state the handlers
    /// accumulated. Shared by the normal and the resume path.
    fn apply_element_result(
        &mut self,
        should_remove_content: bool,
        end_tag_handler: Option<H::EndTagHandler<'static>>,
        current_element_data: Option<&mut ElementDescriptor>,
    ) {
        if self.next_element_can_have_content {
            if let Some(elem_desc) = current_element_data {
                if should_remove_content {
                    elem_desc.remove_content = true;
                    self.matched_elements_with_removed_content += 1;
                }

                if let Some(handler) = end_tag_handler {
                    elem_desc.end_tag_handler_idx = self.end_tag_handlers.push(handler, false);
                }
            }
        }
    }

    pub fn handle_start_tag(
        &mut self,
        start_tag: &mut StartTag<'_>,
        current_element_data: Option<&mut ElementDescriptor>,
    ) -> HandlerResult {
        if self.matched_elements_with_removed_content > 0 {
            start_tag.remove();
        }

        let mut element = Element::new(start_tag, self.next_element_can_have_content);

        let mut next_handler_idx = 0;
        if let Err(e) = self
            .element_handlers
            .do_for_each_active_and_deactivate_from(0, &mut next_handler_idx, |h| h(&mut element))
        {
            if is_suspension_request(&*e) {
                // Deep-copy the element (and the stack-local start tag it
                // borrows) onto the heap so it survives the unwind out of
                // `write()`.
                self.suspended = Some(SuspendedToken::Element {
                    element: Box::new(element.into_suspended()),
                    next_handler_idx,
                });
            }
            return Err(e);
        }

        debug_assert!(!self.next_element_can_have_content || element.can_have_content());
        let should_remove_content = element.should_remove_content();
        let end_tag_handler = element.into_end_tag_handler();
        self.apply_element_result(should_remove_content, end_tag_handler, current_element_data);

        Ok(())
    }

    pub fn handle_token(
        &mut self,
        token: &mut Token<'_>,
        current_element_data: Option<&mut ElementDescriptor>,
    ) -> HandlerResult {
        debug_assert!(
            self.suspended.is_none(),
            "a token was dispatched while a suspension is pending"
        );

        match token {
            Token::StartTag(start_tag) => {
                return self.handle_start_tag(start_tag, current_element_data);
            }
            Token::EndTag(end_tag) => {
                let res = self
                    .end_tag_handlers
                    .do_for_each_active_and_remove_tail(|h| h(end_tag));
                if let Err(e) = res {
                    if is_suspension_request(&*e) {
                        self.suspended =
                            Some(SuspendedToken::EndTag(Box::new(end_tag.take_owned())));
                    }
                    return Err(e);
                }
            }
            Token::Doctype(doctype) => {
                let mut next_handler_idx = 0;
                let res =
                    self.doctype_handlers
                        .for_each_active_from(0, &mut next_handler_idx, |h| h(doctype));
                if let Err(e) = res {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::Doctype {
                            doctype: Box::new(doctype.take_owned()),
                            next_handler_idx,
                        });
                    }
                    return Err(e);
                }
            }
            Token::TextChunk(text) => {
                let mut next_handler_idx = 0;
                let res = self
                    .text_handlers
                    .for_each_active_from(0, &mut next_handler_idx, |h| h(text));
                if let Err(e) = res {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::TextChunk {
                            chunk: Box::new(text.take_owned()),
                            next_handler_idx,
                        });
                    }
                    return Err(e);
                }
            }
            Token::Comment(comment) => {
                let mut next_handler_idx = 0;
                let res =
                    self.comment_handlers
                        .for_each_active_from(0, &mut next_handler_idx, |h| h(comment));
                if let Err(e) = res {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::Comment {
                            comment: Box::new(comment.take_owned()),
                            next_handler_idx,
                        });
                    }
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    pub fn handle_end(&mut self, document_end: &mut DocumentEnd<'_>) -> HandlerResult {
        let res = self
            .end_handlers
            .do_for_each_active_and_remove_tail(|h| h(document_end));
        if let Err(e) = res {
            if is_suspension_request(&*e) {
                self.suspended = Some(SuspendedToken::DocumentEnd(Box::new(
                    document_end.take_owned(),
                )));
            }
            return Err(e);
        }
        Ok(())
    }

    /// The unit a handler is suspended on, if any. The returned reference is
    /// into a `Box`, so its address is stable until the token completes.
    #[inline]
    pub fn suspended_token_mut(&mut self) -> Option<&mut SuspendedToken<H>> {
        self.suspended.as_mut()
    }

    /// Runs the handlers that had not yet run for the parked token and hands
    /// the completed token back for serialization. If another handler
    /// suspends, the (already owned) unit is re-parked at its *same* heap
    /// address and the suspension error propagates.
    ///
    /// Must not be called for a parked `DocumentEnd`; use
    /// [`Self::resume_suspended_document_end`].
    pub fn resume_suspended_token(
        &mut self,
        current_element_data: Option<&mut ElementDescriptor>,
    ) -> Result<Token<'static>, Box<dyn std::error::Error + Send + Sync>> {
        match self
            .suspended
            .take()
            .expect("resume_suspended_token called without a pending suspension")
        {
            SuspendedToken::Element {
                mut element,
                next_handler_idx,
            } => {
                let mut next = next_handler_idx;
                if let Err(e) = self
                    .element_handlers
                    .do_for_each_active_and_deactivate_from(next_handler_idx, &mut next, |h| {
                        h(&mut element)
                    })
                {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::Element {
                            element,
                            next_handler_idx: next,
                        });
                    }
                    return Err(e);
                }

                debug_assert!(!self.next_element_can_have_content || element.can_have_content());
                let (start_tag, should_remove_content, end_tag_handler) =
                    element.into_owned_parts();
                self.apply_element_result(
                    should_remove_content,
                    end_tag_handler,
                    current_element_data,
                );
                Ok(Token::StartTag(*start_tag))
            }
            SuspendedToken::EndTag(mut end_tag) => {
                if let Err(e) = self
                    .end_tag_handlers
                    .do_for_each_active_and_remove_tail(|h| h(&mut end_tag))
                {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::EndTag(end_tag));
                    }
                    return Err(e);
                }
                Ok(Token::EndTag(*end_tag))
            }
            SuspendedToken::TextChunk {
                mut chunk,
                next_handler_idx,
            } => {
                let mut next = next_handler_idx;
                if let Err(e) =
                    self.text_handlers
                        .for_each_active_from(next_handler_idx, &mut next, |h| h(&mut chunk))
                {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::TextChunk {
                            chunk,
                            next_handler_idx: next,
                        });
                    }
                    return Err(e);
                }
                Ok(Token::TextChunk(*chunk))
            }
            SuspendedToken::Comment {
                mut comment,
                next_handler_idx,
            } => {
                let mut next = next_handler_idx;
                if let Err(e) =
                    self.comment_handlers
                        .for_each_active_from(next_handler_idx, &mut next, |h| h(&mut comment))
                {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::Comment {
                            comment,
                            next_handler_idx: next,
                        });
                    }
                    return Err(e);
                }
                Ok(Token::Comment(*comment))
            }
            SuspendedToken::Doctype {
                mut doctype,
                next_handler_idx,
            } => {
                let mut next = next_handler_idx;
                if let Err(e) =
                    self.doctype_handlers
                        .for_each_active_from(next_handler_idx, &mut next, |h| h(&mut doctype))
                {
                    if is_suspension_request(&*e) {
                        self.suspended = Some(SuspendedToken::Doctype {
                            doctype,
                            next_handler_idx: next,
                        });
                    }
                    return Err(e);
                }
                Ok(Token::Doctype(*doctype))
            }
            SuspendedToken::DocumentEnd(_) => {
                unreachable!("a DocumentEnd suspension is resumed through resume_finish")
            }
        }
    }

    /// Runs the remaining document-end handlers for the parked
    /// [`DocumentEnd`]. See [`Self::resume_suspended_token`].
    pub fn resume_suspended_document_end(
        &mut self,
    ) -> Result<DocumentEnd<'static>, Box<dyn std::error::Error + Send + Sync>> {
        let Some(SuspendedToken::DocumentEnd(mut document_end)) = self.suspended.take() else {
            unreachable!("resume_suspended_document_end called without a parked DocumentEnd")
        };

        if let Err(e) = self
            .end_handlers
            .do_for_each_active_and_remove_tail(|h| h(&mut document_end))
        {
            if is_suspension_request(&*e) {
                self.suspended = Some(SuspendedToken::DocumentEnd(document_end));
            }
            return Err(e);
        }

        Ok(*document_end)
    }

    #[inline]
    pub fn get_token_capture_flags(&self) -> TokenCaptureFlags {
        let mut flags = TokenCaptureFlags::empty();

        if self.doctype_handlers.has_active() {
            flags |= TokenCaptureFlags::DOCTYPES;
        }

        if self.comment_handlers.has_active() {
            flags |= TokenCaptureFlags::COMMENTS;
        }

        if self.text_handlers.has_active() {
            flags |= TokenCaptureFlags::TEXT;
        }

        if self.end_tag_handlers.has_active() {
            flags |= TokenCaptureFlags::NEXT_END_TAG;
        }

        if self.element_handlers.has_active() {
            flags |= TokenCaptureFlags::NEXT_START_TAG;
        }

        flags
    }
}

#[test]
fn locator_size() {
    assert!(size_of::<SelectorHandlersLocator>() <= 12);
}
