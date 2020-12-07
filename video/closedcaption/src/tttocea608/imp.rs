// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace, gst_warning};

use once_cell::sync::Lazy;

use crate::ffi;
use std::sync::Mutex;

use crate::ttutils::{Cea608Mode, Chunk, Line, Lines, TextStyle};

fn is_basicna(cc_data: u16) -> bool {
    0x0000 != (0x6000 & cc_data)
}

fn is_westeu(cc_data: u16) -> bool {
    0x1220 == (0x7660 & cc_data)
}

fn is_specialna(cc_data: u16) -> bool {
    0x1130 == (0x7770 & cc_data)
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn eia608_from_utf8_1(c: &[u8; 5]) -> u16 {
    assert!(c[4] == 0);
    unsafe { ffi::eia608_from_utf8_1(c.as_ptr() as *const _, 0) }
}

fn eia608_to_text(cc_data: u16) -> String {
    unsafe {
        let bufsz = ffi::eia608_to_text(std::ptr::null_mut(), 0, cc_data);
        let mut data = Vec::with_capacity((bufsz + 1) as usize);
        data.set_len(bufsz as usize);
        ffi::eia608_to_text(data.as_ptr() as *mut _, (bufsz + 1) as usize, cc_data);
        String::from_utf8_unchecked(data)
    }
}

fn eia608_row_column_preamble(row: i32, col: i32, underline: bool) -> u16 {
    unsafe {
        /* Hardcoded chan */
        ffi::eia608_row_column_pramble(row, col, 0, underline as i32)
    }
}

fn eia608_row_style_preamble(row: i32, style: u32, underline: bool) -> u16 {
    unsafe {
        /* Hardcoded chan */
        ffi::eia608_row_style_pramble(row, 0, style, underline as i32)
    }
}

fn eia608_midrow_change(style: u32, underline: bool) -> u16 {
    unsafe {
        /* Hardcoded chan and underline */
        ffi::eia608_midrow_change(0, style, underline as i32)
    }
}

fn eia608_control_command(cmd: ffi::eia608_control_t) -> u16 {
    unsafe { ffi::eia608_control_command(cmd, 0) }
}

fn eia608_from_basicna(bna1: u16, bna2: u16) -> u16 {
    unsafe { ffi::eia608_from_basicna(bna1, bna2) }
}

fn erase_non_displayed_memory() -> u16 {
    eia608_control_command(ffi::eia608_control_t_eia608_control_erase_non_displayed_memory)
}

const DEFAULT_FPS_N: i32 = 30;
const DEFAULT_FPS_D: i32 = 1;

const DEFAULT_MODE: Cea608Mode = Cea608Mode::RollUp2;

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("mode", |name| {
    glib::ParamSpec::enum_(
        name,
        "Mode",
        "Which mode to operate in",
        Cea608Mode::static_type(),
        DEFAULT_MODE as i32,
        glib::ParamFlags::READWRITE,
    )
})];

#[derive(Debug, Clone)]
struct Settings {
    mode: Cea608Mode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { mode: DEFAULT_MODE }
    }
}

struct State {
    settings: Settings,
    framerate: gst::Fraction,
    erase_display_frame_no: Option<u64>,
    last_frame_no: u64,
    max_frame_no: u64,
    send_roll_up_preamble: bool,
    json_input: bool,
    style: TextStyle,
    underline: bool,
    column: u32,
    mode: Cea608Mode,
}

impl Default for State {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            framerate: gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
            erase_display_frame_no: None,
            last_frame_no: 0,
            max_frame_no: 0,
            column: 0,
            send_roll_up_preamble: false,
            json_input: false,
            style: TextStyle::White,
            underline: false,
            mode: Cea608Mode::PopOn,
        }
    }
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "tttocea608",
        gst::DebugColorFlags::empty(),
        Some("TT CEA 608 Element"),
    )
});

static SPACE: Lazy<u16> = Lazy::new(|| eia608_from_utf8_1(&[0x20, 0, 0, 0, 0]));

fn cc_data_buffer(
    element: &super::TtToCea608,
    cc_data: u16,
    pts: gst::ClockTime,
    duration: gst::ClockTime,
) -> gst::Buffer {
    let mut ret = gst::Buffer::with_size(2).unwrap();
    let buf_mut = ret.get_mut().unwrap();
    let data = cc_data.to_be_bytes();

    if cc_data != 0x8080 {
        gst_log!(
            CAT,
            obj: element,
            "{} -> {}: {}",
            pts,
            pts + duration,
            eia608_to_text(cc_data)
        );
    } else {
        gst_trace!(CAT, obj: element, "{} -> {}: padding", pts, pts + duration);
    }

    buf_mut.copy_from_slice(0, &data).unwrap();
    buf_mut.set_pts(pts);
    buf_mut.set_duration(duration);

    ret
}

impl State {
    fn check_erase_display(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
    ) -> bool {
        if let Some(erase_display_frame_no) = self.erase_display_frame_no {
            if self.last_frame_no == erase_display_frame_no - 1 {
                self.erase_display_frame_no = None;
                self.erase_display_memory(element, bufferlist);
                return true;
            }
        }

        false
    }

    fn cc_data(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        cc_data: u16,
    ) {
        self.check_erase_display(element, bufferlist);

        let (fps_n, fps_d) = (
            *self.framerate.numer() as u64,
            *self.framerate.denom() as u64,
        );

        let pts = (self.last_frame_no * gst::SECOND)
            .mul_div_round(fps_d, fps_n)
            .unwrap();

        if self.last_frame_no < self.max_frame_no {
            self.last_frame_no += 1;
        } else {
            gst_debug!(CAT, obj: element, "More text than bandwidth!");
        }

        let next_pts = (self.last_frame_no * gst::SECOND)
            .mul_div_round(fps_d, fps_n)
            .unwrap();

        let duration = next_pts - pts;

        bufferlist.insert(-1, cc_data_buffer(element, cc_data, pts, duration));
    }

    fn pad(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        frame_no: u64,
    ) {
        while self.last_frame_no < frame_no {
            if !self.check_erase_display(element, bufferlist) {
                self.cc_data(element, bufferlist, 0x8080);
            }
        }
    }

    fn resume_caption_loading(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
    ) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_resume_caption_loading),
        )
    }

    fn roll_up_2(&mut self, element: &super::TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_roll_up_2),
        )
    }

    fn roll_up_3(&mut self, element: &super::TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_roll_up_3),
        )
    }

    fn roll_up_4(&mut self, element: &super::TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_roll_up_4),
        )
    }

    fn carriage_return(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
    ) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_carriage_return),
        )
    }

    fn end_of_caption(&mut self, element: &super::TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_end_of_caption),
        )
    }

    fn tab_offset(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        offset: u32,
    ) {
        match offset {
            0 => (),
            1 => self.cc_data(
                element,
                bufferlist,
                eia608_control_command(ffi::eia608_control_t_eia608_tab_offset_1),
            ),
            2 => self.cc_data(
                element,
                bufferlist,
                eia608_control_command(ffi::eia608_control_t_eia608_tab_offset_2),
            ),
            3 => self.cc_data(
                element,
                bufferlist,
                eia608_control_command(ffi::eia608_control_t_eia608_tab_offset_3),
            ),
            _ => unreachable!(),
        }
    }

    fn preamble_indent(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        row: i32,
        col: i32,
        underline: bool,
    ) {
        self.cc_data(
            element,
            bufferlist,
            eia608_row_column_preamble(row, col, underline),
        )
    }

    fn preamble_style(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        row: i32,
        style: u32,
        underline: bool,
    ) {
        self.cc_data(
            element,
            bufferlist,
            eia608_row_style_preamble(row, style, underline),
        )
    }

    fn midrow_change(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        style: u32,
        underline: bool,
    ) {
        self.cc_data(element, bufferlist, eia608_midrow_change(style, underline))
    }

    fn bna(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        bna1: u16,
        bna2: u16,
    ) {
        self.cc_data(element, bufferlist, eia608_from_basicna(bna1, bna2))
    }

    fn erase_display_memory(
        &mut self,
        element: &super::TtToCea608,
        bufferlist: &mut gst::BufferListRef,
    ) {
        self.cc_data(
            element,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_erase_display_memory),
        )
    }
}

pub struct TtToCea608 {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl TtToCea608 {
    fn open_chunk(
        &self,
        element: &super::TtToCea608,
        state: &mut State,
        chunk: &Chunk,
        bufferlist: &mut gst::BufferListRef,
        col: u32,
    ) -> bool {
        if (chunk.style != state.style || chunk.underline != state.underline) && col < 31 {
            state.midrow_change(element, bufferlist, chunk.style as u32, chunk.underline);
            state.style = chunk.style;
            state.underline = chunk.underline;
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_line(
        &self,
        element: &super::TtToCea608,
        state: &mut State,
        chunk: &Chunk,
        bufferlist: &mut gst::BufferListRef,
        col: &mut u32,
        row: i32,
        carriage_return: Option<bool>,
    ) -> bool {
        let mut ret = true;

        let do_preamble = match state.mode {
            Cea608Mode::PopOn => true,
            Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                if let Some(carriage_return) = carriage_return {
                    if carriage_return {
                        *col = 0;
                        state.carriage_return(element, bufferlist);
                        true
                    } else {
                        state.send_roll_up_preamble
                    }
                } else {
                    state.send_roll_up_preamble
                }
            }
        };

        let mut indent = *col / 4;
        let mut offset = *col % 4;

        if do_preamble {
            match state.mode {
                Cea608Mode::RollUp2 => state.roll_up_2(element, bufferlist),
                Cea608Mode::RollUp3 => state.roll_up_3(element, bufferlist),
                Cea608Mode::RollUp4 => state.roll_up_4(element, bufferlist),
                _ => (),
            }

            if chunk.style != TextStyle::White && indent == 0 {
                state.preamble_style(
                    element,
                    bufferlist,
                    row,
                    chunk.style as u32,
                    chunk.underline,
                );
                state.style = chunk.style;
            } else {
                if chunk.style != TextStyle::White {
                    if offset > 0 {
                        offset -= 1;
                    } else {
                        indent -= 1;
                        offset = 3;
                    }
                    *col -= 1;
                }

                state.style = TextStyle::White;
                state.preamble_indent(
                    element,
                    bufferlist,
                    row,
                    (indent * 4) as i32,
                    chunk.underline,
                );
            }

            state.tab_offset(element, bufferlist, offset);

            state.underline = chunk.underline;
            state.send_roll_up_preamble = false;
            ret = false;
        }

        if self.open_chunk(element, state, chunk, bufferlist, *col) {
            *col += 1;
            ret = false
        }

        ret
    }

    fn generate(
        &self,
        mut state: &mut State,
        element: &super::TtToCea608,
        pts: gst::ClockTime,
        duration: gst::ClockTime,
        lines: Lines,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut row = 13;
        let mut bufferlist = gst::BufferList::new();
        let mut_list = bufferlist.get_mut().unwrap();

        let mut col = if state.mode == Cea608Mode::PopOn {
            0
        } else {
            state.column
        };

        let (fps_n, fps_d) = (
            *state.framerate.numer() as u64,
            *state.framerate.denom() as u64,
        );

        let frame_no = (pts.mul_div_round(fps_n, fps_d).unwrap() / gst::SECOND).unwrap();

        state.max_frame_no =
            ((pts + duration).mul_div_round(fps_n, fps_d).unwrap() / gst::SECOND).unwrap();

        state.pad(element, mut_list, frame_no);

        let mut cleared = false;
        if let Some(mode) = lines.mode {
            if mode != state.mode {
                /* Always erase the display when going to or from pop-on */
                if state.mode == Cea608Mode::PopOn || mode == Cea608Mode::PopOn {
                    state.erase_display_frame_no = None;
                    state.erase_display_memory(element, mut_list);
                    cleared = true;
                }

                state.mode = mode;
                match state.mode {
                    Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                        state.send_roll_up_preamble = true;
                    }
                    _ => col = 0,
                }
            }
        }

        if let Some(clear) = lines.clear {
            if clear && !cleared {
                state.erase_display_frame_no = None;
                state.erase_display_memory(element, mut_list);
                if state.mode != Cea608Mode::PopOn {
                    state.send_roll_up_preamble = true;
                }
                col = 0;
            }
        }

        if state.mode == Cea608Mode::PopOn {
            if state.erase_display_frame_no.is_some() {
                state.erase_display_frame_no = None;
                state.erase_display_memory(element, mut_list);
            }
            state.resume_caption_loading(element, mut_list);
            state.cc_data(element, mut_list, erase_non_displayed_memory());
        }

        let mut prev_char = 0;

        for line in &lines.lines {
            gst_log!(CAT, obj: element, "Processing {:?}", line);

            if let Some(line_row) = line.row {
                row = line_row;
            }

            if row > 14 {
                gst_warning!(
                    CAT,
                    obj: element,
                    "Dropping line after 15th row: {:?}",
                    line
                );
                continue;
            }

            if let Some(line_column) = line.column {
                if state.mode != Cea608Mode::PopOn {
                    state.send_roll_up_preamble = true;
                }
                col = line_column;
            } else if state.mode == Cea608Mode::PopOn {
                col = 0;
            }

            for (j, chunk) in line.chunks.iter().enumerate() {
                let mut prepend_space = true;
                if prev_char != 0 {
                    state.cc_data(element, mut_list, prev_char);
                    prev_char = 0;
                }

                if j == 0 {
                    prepend_space = self.open_line(
                        element,
                        &mut state,
                        chunk,
                        mut_list,
                        &mut col,
                        row as i32,
                        line.carriage_return,
                    );
                } else if self.open_chunk(element, &mut state, chunk, mut_list, col) {
                    prepend_space = false;
                    col += 1;
                }

                let text = {
                    if prepend_space {
                        let mut text = " ".to_string();
                        text.push_str(&chunk.text);
                        text
                    } else {
                        chunk.text.clone()
                    }
                };

                for c in text.chars() {
                    if c == '\r' {
                        continue;
                    }

                    let mut encoded = [0; 5];
                    c.encode_utf8(&mut encoded);
                    let mut cc_data = eia608_from_utf8_1(&encoded);

                    if cc_data == 0 {
                        gst_warning!(CAT, obj: element, "Not translating UTF8: {}", c);
                        cc_data = *SPACE;
                    }

                    if is_basicna(prev_char) {
                        if is_basicna(cc_data) {
                            state.bna(element, mut_list, prev_char, cc_data);
                        } else if is_westeu(cc_data) {
                            // extended characters overwrite the previous character,
                            // so insert a dummy char then write the extended char
                            state.bna(element, mut_list, prev_char, *SPACE);
                            state.cc_data(element, mut_list, cc_data);
                        } else {
                            state.cc_data(element, mut_list, prev_char);
                            state.cc_data(element, mut_list, cc_data);
                        }
                        prev_char = 0;
                    } else if is_westeu(cc_data) {
                        // extended characters overwrite the previous character,
                        // so insert a dummy char then write the extended char
                        state.cc_data(element, mut_list, *SPACE);
                        state.cc_data(element, mut_list, cc_data);
                    } else if is_basicna(cc_data) {
                        prev_char = cc_data;
                    } else {
                        state.cc_data(element, mut_list, cc_data);
                    }

                    if is_specialna(cc_data) {
                        state.resume_caption_loading(element, mut_list);
                    }

                    col += 1;

                    if col > 31 {
                        match state.mode {
                            Cea608Mode::PopOn => {
                                gst_warning!(
                                    CAT,
                                    obj: element,
                                    "Dropping characters after 32nd column: {}",
                                    c
                                );
                                break;
                            }
                            Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                                if prev_char != 0 {
                                    state.cc_data(element, mut_list, prev_char);
                                    prev_char = 0;
                                }

                                self.open_line(
                                    element,
                                    &mut state,
                                    chunk,
                                    mut_list,
                                    &mut col,
                                    row as i32,
                                    Some(true),
                                );
                            }
                        }
                    }
                }
            }

            if state.mode == Cea608Mode::PopOn {
                if prev_char != 0 {
                    state.cc_data(element, mut_list, prev_char);
                    prev_char = 0;
                }
                row += 1;
            }
        }

        if prev_char != 0 {
            state.cc_data(element, mut_list, prev_char);
        }

        if state.mode == Cea608Mode::PopOn {
            state.end_of_caption(element, mut_list);
        }

        state.column = col;

        if state.mode == Cea608Mode::PopOn {
            state.erase_display_frame_no = Some(
                state.last_frame_no
                    + (duration.mul_div_round(fps_n, fps_d).unwrap() / gst::SECOND).unwrap(),
            );
        }

        self.srcpad.push_list(bufferlist)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::TtToCea608,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pts = match buffer.get_pts() {
            gst::CLOCK_TIME_NONE => {
                gst::element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Stream with timestamped buffers required"]
                );
                Err(gst::FlowError::Error)
            }
            pts => Ok(pts),
        }?;

        let duration = match buffer.get_duration() {
            gst::CLOCK_TIME_NONE => {
                gst::element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Buffers of stream need to have a duration"]
                );
                Err(gst::FlowError::Error)
            }
            duration => Ok(duration),
        }?;

        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: pad, "Can't map buffer readable");

            gst::FlowError::Error
        })?;

        let mut state = self.state.lock().unwrap();

        let mut lines = Lines {
            lines: Vec::new(),
            mode: None,
            clear: None,
        };
        match state.json_input {
            false => {
                let data = std::str::from_utf8(&data).map_err(|err| {
                    gst_error!(CAT, obj: pad, "Can't decode utf8: {}", err);

                    gst::FlowError::Error
                })?;

                let phrases: Vec<&str> = data.split('\n').collect();
                let mut row = (std::cmp::max(0, 15 - phrases.len())) as u32;

                for phrase in &phrases {
                    lines.lines.push(Line {
                        carriage_return: None,
                        column: None,
                        row: Some(row),
                        chunks: vec![Chunk {
                            style: TextStyle::White,
                            underline: false,
                            text: phrase.to_string(),
                        }],
                    });
                    row += 1;
                }
            }
            true => {
                lines = serde_json::from_slice(&data).map_err(|err| {
                    gst_error!(CAT, obj: pad, "Failed to parse input as json: {}", err);

                    gst::FlowError::Error
                })?;
            }
        }

        self.generate(&mut state, element, pts, duration, lines)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::TtToCea608, event: gst::Event) -> bool {
        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        use gst::EventView;

        match event.view() {
            EventView::Caps(e) => {
                let mut downstream_caps = match self.srcpad.get_allowed_caps() {
                    None => self.srcpad.get_pad_template_caps(),
                    Some(caps) => caps,
                };

                if downstream_caps.is_empty() {
                    gst_error!(CAT, obj: pad, "Empty downstream caps");
                    return false;
                }

                let caps = downstream_caps.make_mut();
                let s = caps.get_mut_structure(0).unwrap();

                s.fixate_field_nearest_fraction(
                    "framerate",
                    gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
                );
                s.fixate();

                let mut state = self.state.lock().unwrap();
                state.framerate = s.get_some::<gst::Fraction>("framerate").unwrap();

                let upstream_caps = e.get_caps();
                let s = upstream_caps.get_structure(0).unwrap();
                state.json_input = s.get_name() == "application/x-json";

                gst_debug!(CAT, obj: pad, "Pushing caps {}", caps);

                let new_event = gst::event::Caps::new(&downstream_caps);

                drop(state);

                self.srcpad.push_event(new_event)
            }
            EventView::Gap(e) => {
                let mut state = self.state.lock().unwrap();

                let (fps_n, fps_d) = (
                    *state.framerate.numer() as u64,
                    *state.framerate.denom() as u64,
                );

                let (timestamp, duration) = e.get();
                let frame_no = ((timestamp + duration).mul_div_round(fps_n, fps_d).unwrap()
                    / gst::SECOND)
                    .unwrap();
                state.max_frame_no = frame_no;

                let mut bufferlist = gst::BufferList::new();
                let mut_list = bufferlist.get_mut().unwrap();

                state.pad(element, mut_list, frame_no);

                drop(state);

                let _ = self.srcpad.push_list(bufferlist);

                true
            }
            EventView::Eos(_) => {
                let mut state = self.state.lock().unwrap();
                if let Some(erase_display_frame_no) = state.erase_display_frame_no {
                    let mut bufferlist = gst::BufferList::new();
                    let mut_list = bufferlist.get_mut().unwrap();

                    state.max_frame_no = erase_display_frame_no;
                    state.pad(element, mut_list, erase_display_frame_no);

                    drop(state);

                    let _ = self.srcpad.push_list(bufferlist);
                }
                pad.event_default(Some(element), event)
            }
            EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();

                *state = State::default();
                state.settings = self.settings.lock().unwrap().clone();
                state.mode = state.settings.mode;

                if state.mode != Cea608Mode::PopOn {
                    state.send_roll_up_preamble = true;
                }

                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }
}

impl ObjectSubclass for TtToCea608 {
    const NAME: &'static str = "TtToCea608";
    type Type = super::TtToCea608;
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::object_subclass!();

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                TtToCea608::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this, element| this.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TtToCea608::catch_panic_pad_function(
                    parent,
                    || false,
                    |this, element| this.sink_event(pad, element, event),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "TT to CEA-608",
            "Generic",
            "Converts timed text to CEA-608 Closed Captions",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();

            let s = gst::Structure::new_empty("text/x-raw");
            caps.append_structure(s);

            let s = gst::Structure::builder("application/x-json")
                .field("format", &"cea608")
                .build();
            caps.append_structure(s);
        }

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let framerate = gst::FractionRange::new(
            gst::Fraction::new(1, std::i32::MAX),
            gst::Fraction::new(std::i32::MAX, 1),
        );

        let caps = gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field("framerate", &framerate)
            .build();

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for TtToCea608 {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("mode", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.mode = value
                    .get_some::<Cea608Mode>()
                    .expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("mode", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.mode.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for TtToCea608 {
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                let settings = self.settings.lock().unwrap();
                *state = State::default();
                state.settings = settings.clone();
                state.mode = state.settings.mode;
                if state.mode != Cea608Mode::PopOn {
                    state.send_roll_up_preamble = true;
                }
            }
            _ => (),
        }

        let ret = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        Ok(ret)
    }
}
