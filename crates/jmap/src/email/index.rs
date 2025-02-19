/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::borrow::Cow;

use jmap_proto::{
    object::Object,
    types::{
        date::UTCDate,
        keyword::Keyword,
        property::{HeaderForm, Property},
        value::Value,
    },
};
use mail_parser::{
    decoders::html::html_to_text,
    parsers::{fields::thread::thread_name, preview::preview_text},
    Addr, Address, GetHeader, Group, HeaderName, HeaderValue, Message, MessagePart, PartType,
};
use nlp::language::Language;
use store::{
    fts::builder::{FtsIndexBuilder, MAX_TOKEN_LENGTH},
    write::{BatchBuilder, IntoOperations, F_BITMAP, F_CLEAR, F_INDEX, F_VALUE},
};

use crate::email::headers::IntoForm;

pub const MAX_MESSAGE_PARTS: usize = 1000;
pub const MAX_ID_LENGTH: usize = 100;
pub const MAX_SORT_FIELD_LENGTH: usize = 255;
pub const MAX_STORED_FIELD_LENGTH: usize = 512;
pub const PREVIEW_LENGTH: usize = 256;

#[derive(Debug)]
pub struct SortedAddressBuilder {
    last_is_space: bool,
    pub buf: String,
}

pub(super) trait IndexMessage {
    fn index_message(
        &mut self,
        message: Message,
        keywords: Vec<Keyword>,
        mailbox_ids: Vec<u32>,
        received_at: u64,
        default_language: Language,
    ) -> store::Result<&mut Self>;
}

impl IndexMessage for BatchBuilder {
    fn index_message(
        &mut self,
        message: Message,
        keywords: Vec<Keyword>,
        mailbox_ids: Vec<u32>,
        received_at: u64,
        default_language: Language,
    ) -> store::Result<&mut Self> {
        let mut metadata = Object::with_capacity(15);

        // Index keywords
        self.value(Property::Keywords, keywords, F_VALUE | F_BITMAP);

        // Index mailboxIds
        self.value(Property::MailboxIds, mailbox_ids, F_VALUE | F_BITMAP);

        // Index size
        metadata.append(Property::Size, message.raw_message.len());
        self.value(Property::Size, message.raw_message.len() as u32, F_INDEX)
            .quota(message.raw_message.len() as i64);

        // Index receivedAt
        metadata.append(
            Property::ReceivedAt,
            Value::Date(UTCDate::from_timestamp(received_at as i64)),
        );
        self.value(Property::ReceivedAt, received_at, F_INDEX);

        let mut fts = FtsIndexBuilder::with_default_language(default_language);
        let mut seen_headers = [false; 40];
        let mut language = Language::Unknown;
        let mut has_attachments = false;
        let preview_part_id = message
            .text_body
            .first()
            .or_else(|| message.html_body.first())
            .copied()
            .unwrap_or(usize::MAX);

        for (part_id, part) in message
            .parts
            .into_iter()
            .take(MAX_MESSAGE_PARTS)
            .enumerate()
        {
            let part_language = part.language().unwrap_or(language);
            if part_id == 0 {
                language = part_language;
                let mut extra_ids = Vec::new();
                for header in part.headers.into_iter().rev() {
                    if matches!(header.name, HeaderName::Other(_)) {
                        continue;
                    }
                    // Index hasHeader property
                    let header_num = header.name.id().to_string();
                    fts.index_raw_token(Property::Headers, &header_num);

                    match header.name {
                        HeaderName::MessageId
                        | HeaderName::InReplyTo
                        | HeaderName::References
                        | HeaderName::ResentMessageId => {
                            header.value.visit_text(|id| {
                                // Add ids to inverted index
                                if id.len() < MAX_ID_LENGTH {
                                    self.value(Property::MessageId, id, F_INDEX);
                                }

                                // Index ids without stemming
                                if id.len() < MAX_TOKEN_LENGTH {
                                    fts.index_raw_token(
                                        Property::Headers,
                                        format!("{header_num}{id}"),
                                    );
                                }
                            });

                            if matches!(
                                header.name,
                                HeaderName::MessageId
                                    | HeaderName::InReplyTo
                                    | HeaderName::References
                            ) && !seen_headers[header.name.id() as usize]
                            {
                                metadata.append(
                                    Property::from_header(&header.name),
                                    header
                                        .value
                                        .trim_text(MAX_STORED_FIELD_LENGTH)
                                        .into_form(&HeaderForm::MessageIds),
                                );
                                seen_headers[header.name.id() as usize] = true;
                            } else {
                                header.value.into_visit_text(|id| {
                                    extra_ids.push(Value::Text(id));
                                });
                            }
                        }
                        HeaderName::From
                        | HeaderName::To
                        | HeaderName::Cc
                        | HeaderName::Bcc
                        | HeaderName::ReplyTo
                        | HeaderName::Sender => {
                            let property = Property::from_header(&header.name);
                            let seen_header = seen_headers[header.name.id() as usize];
                            if matches!(
                                header.name,
                                HeaderName::From
                                    | HeaderName::To
                                    | HeaderName::Cc
                                    | HeaderName::Bcc
                            ) {
                                let mut sort_text = SortedAddressBuilder::new();
                                let mut found_addr = seen_header;

                                header.value.visit_addresses(|element, value| {
                                    if !found_addr {
                                        match element {
                                            AddressElement::Name => {
                                                found_addr = !sort_text.push(value);
                                            }
                                            AddressElement::Address => {
                                                sort_text.push(value);
                                                found_addr = true;
                                            }
                                            AddressElement::GroupName => (),
                                        }
                                    }

                                    // Index an address name or email without stemming
                                    fts.index_raw(u8::from(&property), value);
                                });

                                if !seen_header {
                                    // Add address to inverted index
                                    self.value(u8::from(&property), sort_text.build(), F_INDEX);
                                }
                            }

                            if !seen_header {
                                // Add address to metadata
                                metadata.append(
                                    property,
                                    header
                                        .value
                                        .trim_text(MAX_STORED_FIELD_LENGTH)
                                        .into_form(&HeaderForm::Addresses),
                                );
                                seen_headers[header.name.id() as usize] = true;
                            }
                        }
                        HeaderName::Date => {
                            if !seen_headers[header.name.id() as usize] {
                                if let HeaderValue::DateTime(datetime) = &header.value {
                                    self.value(
                                        Property::SentAt,
                                        datetime.to_timestamp() as u64,
                                        F_INDEX,
                                    );
                                }
                                metadata.append(
                                    Property::SentAt,
                                    header.value.into_form(&HeaderForm::Date),
                                );
                                seen_headers[header.name.id() as usize] = true;
                            }
                        }
                        HeaderName::Subject => {
                            // Index subject
                            let subject = match &header.value {
                                HeaderValue::Text(text) => text.clone(),
                                HeaderValue::TextList(list) if !list.is_empty() => {
                                    list.first().unwrap().clone()
                                }
                                _ => "".into(),
                            };

                            if !seen_headers[header.name.id() as usize] {
                                // Add to metadata
                                metadata.append(
                                    Property::Subject,
                                    header
                                        .value
                                        .trim_text(MAX_STORED_FIELD_LENGTH)
                                        .into_form(&HeaderForm::Text),
                                );

                                // Index thread name
                                let thread_name = thread_name(&subject);
                                self.value(
                                    Property::Subject,
                                    if !thread_name.is_empty() {
                                        thread_name.trim_text(MAX_SORT_FIELD_LENGTH)
                                    } else {
                                        "!"
                                    },
                                    F_INDEX,
                                );

                                seen_headers[header.name.id() as usize] = true;
                            }

                            // Index subject for FTS
                            fts.index(Property::Subject, subject, language);
                        }

                        HeaderName::Comments | HeaderName::Keywords | HeaderName::ListId => {
                            // Index headers
                            header.value.visit_text(|text| {
                                for token in text.split_ascii_whitespace() {
                                    if token.len() < MAX_TOKEN_LENGTH {
                                        fts.index_raw_token(
                                            Property::Headers,
                                            format!("{header_num}{}", token.to_lowercase()),
                                        );
                                    }
                                }
                            });
                        }
                        _ => (),
                    }
                }

                // Add any extra Ids to metadata
                if !extra_ids.is_empty() {
                    metadata.append(Property::EmailIds, Value::List(extra_ids));
                }
            }

            // Add subject to index if missing
            if !seen_headers[HeaderName::Subject.id() as usize] {
                self.value(Property::Subject, "!", F_INDEX);
            }

            match part.body {
                PartType::Text(text) => {
                    if part_id == preview_part_id {
                        metadata.append(
                            Property::Preview,
                            preview_text(text.replace('\r', "").into(), PREVIEW_LENGTH),
                        );
                    }

                    if message.text_body.contains(&part_id) || message.html_body.contains(&part_id)
                    {
                        fts.index(Property::TextBody, text, part_language);
                    } else {
                        fts.index(Property::Attachments, text, part_language);
                        has_attachments = true;
                    }
                }
                PartType::Html(html) => {
                    let text = html_to_text(&html);
                    if part_id == preview_part_id {
                        metadata.append(
                            Property::Preview,
                            preview_text(text.replace('\r', "").into(), PREVIEW_LENGTH),
                        );
                    }

                    if message.text_body.contains(&part_id) || message.html_body.contains(&part_id)
                    {
                        fts.index(Property::TextBody, text, part_language);
                    } else {
                        fts.index(Property::Attachments, text, part_language);
                        has_attachments = true;
                    }
                }
                PartType::Binary(_) if !has_attachments => {
                    has_attachments = true;
                }
                PartType::Message(mut nested_message) => {
                    let nested_message_language = nested_message
                        .root_part()
                        .language()
                        .unwrap_or(Language::Unknown);
                    if let Some(HeaderValue::Text(subject)) =
                        nested_message.remove_header(HeaderName::Subject)
                    {
                        fts.index(
                            Property::Attachments,
                            subject.into_owned(),
                            nested_message_language,
                        );
                    }

                    for sub_part in nested_message.parts.into_iter().take(MAX_MESSAGE_PARTS) {
                        let language = sub_part.language().unwrap_or(nested_message_language);
                        match sub_part.body {
                            PartType::Text(text) => {
                                fts.index(Property::Attachments, text, language);
                            }
                            PartType::Html(html) => {
                                fts.index(Property::Attachments, html_to_text(&html), language);
                            }
                            _ => (),
                        }
                    }

                    if !has_attachments {
                        has_attachments = true;
                    }
                }
                _ => {}
            }
        }

        // Store and index hasAttachment property
        metadata.append(Property::HasAttachment, has_attachments);
        if has_attachments {
            self.bitmap(Property::HasAttachment, (), 0);
        }

        // Store properties
        self.value(Property::BodyStructure, metadata, F_VALUE);

        // Store full text index
        self.custom(fts);

        Ok(self)
    }
}

pub struct EmailIndexBuilder {
    inner: Object<Value>,
    set: bool,
}

impl EmailIndexBuilder {
    pub fn set(inner: Object<Value>) -> Self {
        Self { inner, set: true }
    }

    pub fn clear(inner: Object<Value>) -> Self {
        Self { inner, set: false }
    }
}

impl IntoOperations for EmailIndexBuilder {
    fn build(self, batch: &mut BatchBuilder) {
        let options = if self.set {
            // Serialize metadata
            batch.value(Property::BodyStructure, &self.inner, F_VALUE);
            0
        } else {
            // Delete metadata
            batch.value(Property::BodyStructure, (), F_VALUE | F_CLEAR);
            F_CLEAR
        };

        // Remove properties from index
        let mut has_subject = false;
        for (property, value) in self.inner.properties {
            match (&property, value) {
                (Property::Size, Value::UnsignedInt(size)) => {
                    batch
                        .value(Property::Size, size as u32, F_INDEX | options)
                        .quota(if self.set {
                            size as i64
                        } else {
                            -(size as i64)
                        });
                }
                (Property::ReceivedAt | Property::SentAt, Value::Date(date)) => {
                    batch.value(property, date.timestamp() as u64, F_INDEX | options);
                }
                (
                    Property::MessageId
                    | Property::InReplyTo
                    | Property::References
                    | Property::EmailIds,
                    Value::List(ids),
                ) => {
                    // Remove messageIds from index
                    for id in ids {
                        match id {
                            Value::Text(id) if id.len() < MAX_ID_LENGTH => {
                                batch.value(Property::MessageId, id, F_INDEX | options);
                            }
                            _ => {}
                        }
                    }
                }
                (
                    Property::From | Property::To | Property::Cc | Property::Bcc,
                    Value::List(addresses),
                ) => {
                    let mut sort_text = SortedAddressBuilder::new();
                    'outer: for addr in addresses {
                        if let Some(addr) = addr.try_unwrap_object() {
                            for part in [Property::Name, Property::Email] {
                                if let Some(Value::Text(value)) = addr.properties.get(&part) {
                                    if !sort_text.push(value) || part == Property::Email {
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                    batch.value(property, sort_text.build(), F_INDEX | options);
                }
                (Property::Subject, Value::Text(value)) => {
                    let thread_name = thread_name(&value);
                    batch.value(
                        Property::Subject,
                        if !thread_name.is_empty() {
                            thread_name.trim_text(MAX_SORT_FIELD_LENGTH)
                        } else {
                            "!"
                        },
                        F_INDEX | options,
                    );
                    has_subject = true;
                }
                (Property::HasAttachment, Value::Bool(true)) => {
                    batch.bitmap(Property::HasAttachment, (), options);
                }
                _ => {}
            }
        }

        if !has_subject {
            batch.value(Property::Subject, "!", F_INDEX | options);
        }
    }
}

impl SortedAddressBuilder {
    pub fn new() -> Self {
        Self {
            last_is_space: true,
            buf: String::with_capacity(32),
        }
    }

    pub fn push(&mut self, text: &str) -> bool {
        if !text.is_empty() {
            if !self.buf.is_empty() {
                self.buf.push(' ');
                self.last_is_space = true;
            }
            for ch in text.chars() {
                for ch in ch.to_lowercase() {
                    if self.buf.len() < MAX_SORT_FIELD_LENGTH {
                        let is_space = ch.is_whitespace();
                        if !is_space || !self.last_is_space {
                            self.buf.push(ch);
                            self.last_is_space = is_space;
                        }
                    } else {
                        return false;
                    }
                }
            }
        }
        true
    }

    pub fn build(self) -> String {
        if !self.buf.is_empty() {
            self.buf
        } else {
            "!".to_string()
        }
    }
}

impl Default for SortedAddressBuilder {
    fn default() -> Self {
        Self::new()
    }
}

trait GetContentLanguage {
    fn language(&self) -> Option<Language>;
}

impl GetContentLanguage for MessagePart<'_> {
    fn language(&self) -> Option<Language> {
        self.headers
            .header_value(&HeaderName::ContentLanguage)
            .and_then(|v| {
                Language::from_iso_639(match v {
                    HeaderValue::Text(v) => v.as_ref(),
                    HeaderValue::TextList(v) => v.first()?,
                    _ => {
                        return None;
                    }
                })
                .unwrap_or(Language::Unknown)
                .into()
            })
    }
}

trait VisitValues {
    fn visit_addresses(&self, visitor: impl FnMut(AddressElement, &str));
    fn visit_text(&self, visitor: impl FnMut(&str));
    fn into_visit_text(self, visitor: impl FnMut(String));
}

#[derive(Debug, PartialEq, Eq)]
enum AddressElement {
    Name,
    Address,
    GroupName,
}

impl VisitValues for HeaderValue<'_> {
    fn visit_addresses(&self, mut visitor: impl FnMut(AddressElement, &str)) {
        match self {
            HeaderValue::Address(Address::List(addr_list)) => {
                for addr in addr_list {
                    if let Some(name) = &addr.name {
                        visitor(AddressElement::Name, name);
                    }
                    if let Some(addr) = &addr.address {
                        visitor(AddressElement::Address, addr);
                    }
                }
            }
            HeaderValue::Address(Address::Group(groups)) => {
                for group in groups {
                    if let Some(name) = &group.name {
                        visitor(AddressElement::GroupName, name);
                    }

                    for addr in &group.addresses {
                        if let Some(name) = &addr.name {
                            visitor(AddressElement::Name, name);
                        }
                        if let Some(addr) = &addr.address {
                            visitor(AddressElement::Address, addr);
                        }
                    }
                }
            }
            _ => (),
        }
    }

    fn visit_text(&self, mut visitor: impl FnMut(&str)) {
        match &self {
            HeaderValue::Text(text) => {
                visitor(text.as_ref());
            }
            HeaderValue::TextList(texts) => {
                for text in texts {
                    visitor(text.as_ref());
                }
            }
            _ => (),
        }
    }

    fn into_visit_text(self, mut visitor: impl FnMut(String)) {
        match self {
            HeaderValue::Text(text) => {
                visitor(text.into_owned());
            }
            HeaderValue::TextList(texts) => {
                for text in texts {
                    visitor(text.into_owned());
                }
            }
            _ => (),
        }
    }
}

pub trait TrimTextValue {
    fn trim_text(self, length: usize) -> Self;
}

impl TrimTextValue for HeaderValue<'_> {
    fn trim_text(self, length: usize) -> Self {
        match self {
            HeaderValue::Address(Address::List(v)) => {
                HeaderValue::Address(Address::List(v.trim_text(length)))
            }
            HeaderValue::Address(Address::Group(v)) => {
                HeaderValue::Address(Address::Group(v.trim_text(length)))
            }
            HeaderValue::Text(v) => HeaderValue::Text(v.trim_text(length)),
            HeaderValue::TextList(v) => HeaderValue::TextList(v.trim_text(length)),
            v => v,
        }
    }
}

impl TrimTextValue for Addr<'_> {
    fn trim_text(self, length: usize) -> Self {
        Self {
            name: self.name.map(|v| v.trim_text(length)),
            address: self.address.map(|v| v.trim_text(length)),
        }
    }
}

impl TrimTextValue for Group<'_> {
    fn trim_text(self, length: usize) -> Self {
        Self {
            name: self.name.map(|v| v.trim_text(length)),
            addresses: self.addresses.trim_text(length),
        }
    }
}

impl TrimTextValue for Cow<'_, str> {
    fn trim_text(self, length: usize) -> Self {
        if self.len() < length {
            self
        } else {
            match self {
                Cow::Borrowed(v) => v.trim_text(length).into(),
                Cow::Owned(v) => v.trim_text(length).into(),
            }
        }
    }
}

impl TrimTextValue for &str {
    fn trim_text(self, length: usize) -> Self {
        if self.len() < length {
            self
        } else {
            let mut index = 0;

            for (i, _) in self.char_indices() {
                if i > length {
                    break;
                }
                index = i;
            }

            &self[..index]
        }
    }
}

impl TrimTextValue for String {
    fn trim_text(self, length: usize) -> Self {
        if self.len() < length {
            self
        } else {
            let mut result = String::with_capacity(length);
            for (i, c) in self.char_indices() {
                if i > length {
                    break;
                }
                result.push(c);
            }
            result
        }
    }
}

impl<T: TrimTextValue> TrimTextValue for Vec<T> {
    fn trim_text(self, length: usize) -> Self {
        self.into_iter().map(|v| v.trim_text(length)).collect()
    }
}
