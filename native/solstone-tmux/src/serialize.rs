// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::io::{self, Write};

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter};

use crate::model::{CaptureResult, PaneInfo, WindowInfo};

#[derive(Serialize)]
struct TerminalActivity<'a> {
    v: u8,
    t: &'static str,
    kind: &'static str,
    method: &'static str,
    ts: i64,
    end_ts: i64,
    session: &'a str,
    window: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exe: Option<&'a str>,
}

#[derive(Serialize)]
struct AvailabilityActivity {
    v: u8,
    t: &'static str,
    kind: &'static str,
    capability: &'static str,
    status: &'static str,
    method: &'static str,
    ts: i64,
    end_ts: i64,
}

pub fn serialize_terminal_activity(
    ts: i64,
    session: &str,
    window: &str,
    pane_title: Option<&str>,
    cwd: Option<&str>,
    exe: Option<&str>,
) -> Vec<u8> {
    serialize_activity_line(&TerminalActivity {
        v: 1,
        t: "desktop_activity",
        kind: "terminal",
        method: "tmux",
        ts,
        end_ts: ts,
        session,
        window,
        pane_title,
        cwd,
        exe,
    })
}

pub fn serialize_availability_activity(ts: i64, status: &'static str) -> Vec<u8> {
    serialize_activity_line(&AvailabilityActivity {
        v: 1,
        t: "desktop_activity",
        kind: "availability",
        capability: "terminal",
        status,
        method: "tmux",
        ts,
        end_ts: ts,
    })
}

fn serialize_activity_line(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("activity row is serializable");
    bytes.push(b'\n');
    bytes
}

#[derive(Serialize)]
struct Envelope<'a> {
    frame_id: u64,
    timestamp: f64,
    requests: [(); 0],
    analysis: Analysis<'a>,
    content: Content<'a>,
}

#[derive(Serialize)]
struct Analysis<'a> {
    visual_description: String,
    primary: &'a str,
    secondary: &'a str,
    overlap: bool,
}

#[derive(Serialize)]
struct Content<'a> {
    tmux: TmuxContent<'a>,
}

#[derive(Serialize)]
struct TmuxContent<'a> {
    session: &'a str,
    window: ActiveWindow<'a>,
    windows: Vec<Window<'a>>,
    panes: Vec<Pane<'a>>,
}

#[derive(Serialize)]
struct ActiveWindow<'a> {
    id: &'a str,
    index: u32,
    name: &'a str,
}

#[derive(Serialize)]
struct Window<'a> {
    id: &'a str,
    index: u32,
    name: &'a str,
    active: bool,
}

impl<'a> From<&'a WindowInfo> for Window<'a> {
    fn from(window: &'a WindowInfo) -> Self {
        Self {
            id: &window.id,
            index: window.index,
            name: &window.name,
            active: window.active,
        }
    }
}

#[derive(Serialize)]
struct Pane<'a> {
    id: &'a str,
    index: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    active: bool,
    cursor_x: u32,
    cursor_y: u32,
    cursor_visible: bool,
    alternate_on: bool,
    pane_in_mode: bool,
    content: &'a str,
}

impl<'a> From<&'a PaneInfo> for Pane<'a> {
    fn from(pane: &'a PaneInfo) -> Self {
        Self {
            id: &pane.id,
            index: pane.index,
            left: pane.left,
            top: pane.top,
            width: pane.width,
            height: pane.height,
            active: pane.active,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            cursor_visible: pane.cursor_visible,
            alternate_on: pane.alternate_on,
            pane_in_mode: pane.pane_in_mode,
            content: &pane.content,
        }
    }
}

pub fn serialize_frame(
    result: &CaptureResult,
    frame_id: u64,
    timestamp: f64,
) -> Result<Vec<u8>, serde_json::Error> {
    let pane_word = if result.panes.len() == 1 {
        "pane"
    } else {
        "panes"
    };
    let visual_description = format!(
        "Terminal session '{}' with {} {} in window '{}'",
        result.session,
        result.panes.len(),
        pane_word,
        result.window.name
    );
    let envelope = Envelope {
        frame_id,
        timestamp,
        requests: [],
        analysis: Analysis {
            visual_description,
            primary: "tmux",
            secondary: "none",
            overlap: false,
        },
        content: Content {
            tmux: TmuxContent {
                session: &result.session,
                window: ActiveWindow {
                    id: &result.window.id,
                    index: result.window.index,
                    name: &result.window.name,
                },
                windows: result.windows.iter().map(Window::from).collect(),
                panes: result.panes.iter().map(Pane::from).collect(),
            },
        },
    };
    let mut bytes = Vec::new();
    {
        let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, SpacedFormatter);
        envelope.serialize(&mut serializer)?;
    }
    bytes.push(b'\n');
    Ok(bytes)
}

// The journal's envelope encoding separates items with ", " and keys with ": ".
// That spacing is part of the on-the-wire bytes, pinned by the golden envelope
// fixtures, so it is a format contract rather than a style choice.
struct SpacedFormatter;

impl Formatter for SpacedFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        if !first {
            writer.write_all(b", ")?;
        }
        Ok(())
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        if !first {
            writer.write_all(b", ")?;
        }
        Ok(())
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        writer.write_all(b": ")
    }

    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        for character in fragment.chars() {
            if character.is_ascii() {
                writer.write_all(&[character as u8])?;
                continue;
            }
            let codepoint = character as u32;
            if codepoint <= 0xffff {
                write!(writer, "\\u{codepoint:04x}")?;
            } else {
                let adjusted = codepoint - 0x1_0000;
                let high = 0xd800 + (adjusted >> 10);
                let low = 0xdc00 + (adjusted & 0x3ff);
                write!(writer, "\\u{high:04x}\\u{low:04x}")?;
            }
        }
        Ok(())
    }

    fn write_char_escape<W>(&mut self, writer: &mut W, escape: CharEscape) -> io::Result<()>
    where
        W: ?Sized + Write,
    {
        match escape {
            CharEscape::Quote => writer.write_all(b"\\\""),
            CharEscape::ReverseSolidus => writer.write_all(b"\\\\"),
            CharEscape::Solidus => writer.write_all(b"/"),
            CharEscape::Backspace => writer.write_all(b"\\b"),
            CharEscape::FormFeed => writer.write_all(b"\\f"),
            CharEscape::LineFeed => writer.write_all(b"\\n"),
            CharEscape::CarriageReturn => writer.write_all(b"\\r"),
            CharEscape::Tab => writer.write_all(b"\\t"),
            CharEscape::AsciiControl(byte) => write!(writer, "\\u{byte:04x}"),
        }
    }
}
