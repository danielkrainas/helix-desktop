// Copyright 2026 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit_atspi_common::PlatformNode;
use zbus::{fdo, interface};

pub(crate) struct EditableTextInterface(PlatformNode);

impl EditableTextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self(node)
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| crate::util::map_error_from_node(&self.0, error)
    }
}

#[interface(name = "org.a11y.atspi.EditableText")]
impl EditableTextInterface {
    fn set_text_contents(&self, new_contents: &str) -> fdo::Result<bool> {
        self.0
            .set_text_contents(new_contents)
            .map_err(self.map_error())
    }

    // Helix: inserting and deleting are a selection of the range changed
    // (`SetTextSelection`) followed by the whole new text (`SetValue`), so
    // that the application can apply the change to text it shows masked.
    fn insert_text(&self, position: i32, text: &str, length: i32) -> fdo::Result<bool> {
        let current = self.0.text(0, -1).map_err(self.map_error())?;
        let count = current.chars().count() as i32;
        let position = position.clamp(0, count);
        let inserted: String = if length < 0 { text.to_owned() } else { text.chars().take(length as usize).collect() };
        let at = current.char_indices().nth(position as usize).map_or(current.len(), |(i, _)| i);
        let contents = format!("{}{}{}", &current[..at], inserted, &current[at..]);
        self.0.set_selection(0, position, position).map_err(self.map_error())?;
        self.0.set_text_contents(&contents).map_err(self.map_error())
    }

    // Helix: the clipboard operations select their range, then ask the
    // application (a custom action) to copy, cut or paste there; it
    // refuses copying and cutting a password.
    fn copy_text(&self, start_pos: i32, end_pos: i32) -> fdo::Result<()> {
        self.select(start_pos, end_pos)?;
        self.0.clipboard_action(accesskit_atspi_common::CLIPBOARD_COPY).map_err(self.map_error())?;
        Ok(())
    }

    fn cut_text(&self, start_pos: i32, end_pos: i32) -> fdo::Result<bool> {
        self.select(start_pos, end_pos)?;
        self.0.clipboard_action(accesskit_atspi_common::CLIPBOARD_CUT).map_err(self.map_error())
    }

    fn delete_text(&self, start_pos: i32, end_pos: i32) -> fdo::Result<bool> {
        let current = self.0.text(0, -1).map_err(self.map_error())?;
        let count = current.chars().count() as i32;
        let end_pos = if end_pos < 0 { count } else { end_pos.min(count) };
        let start_pos = start_pos.clamp(0, end_pos);
        let byte = |n: i32| current.char_indices().nth(n as usize).map_or(current.len(), |(i, _)| i);
        let contents = format!("{}{}", &current[..byte(start_pos)], &current[byte(end_pos)..]);
        self.0.set_selection(0, start_pos, end_pos).map_err(self.map_error())?;
        self.0.set_text_contents(&contents).map_err(self.map_error())
    }

    fn paste_text(&self, position: i32) -> fdo::Result<bool> {
        self.select(position, position)?;
        self.0.clipboard_action(accesskit_atspi_common::CLIPBOARD_PASTE).map_err(self.map_error())
    }
}

impl EditableTextInterface {
    /// Select characters `start`..`end` (clamped to the text; `end` < 0:
    /// to its end).
    fn select(&self, start: i32, end: i32) -> fdo::Result<()> {
        let count = self.0.text(0, -1).map_err(self.map_error())?.chars().count() as i32;
        let end = if end < 0 { count } else { end.min(count) };
        let start = start.clamp(0, end);
        self.0.set_selection(0, start, end).map_err(self.map_error())?;
        Ok(())
    }
}
