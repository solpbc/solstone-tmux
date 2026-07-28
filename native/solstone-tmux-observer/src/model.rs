// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInfo {
    pub session: String,
    pub activity: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowInfo {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneInfo {
    pub id: String,
    pub index: u32,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureResult {
    pub session: String,
    pub window: WindowInfo,
    pub windows: Vec<WindowInfo>,
    pub panes: Vec<PaneInfo>,
}
