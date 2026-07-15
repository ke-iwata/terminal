use super::color::Color;
use super::grid::CellFlags;
use super::Term;
use vte::{Params, ParamsIter, Perform};

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return(),
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            0x08 => {
                // Backspace: move left one column, no wrap.
                self.wrap_pending_clear_and_move_left();
            }
            b'\t' => self.tab_forward(),
            0x07 => {} // Bell: no-op, no audio/visual bell in the MVP.
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 and 2 both set the window title.
        if params.len() >= 2 && (params[0] == b"0" || params[0] == b"2") {
            self.title = String::from_utf8_lossy(params[1]).into_owned();
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if intermediates.first() == Some(&b'?') {
            self.csi_private_mode(params, action);
            return;
        }

        let mut iter = params.iter();
        let p = |iter: &mut ParamsIter, default: u16| -> u16 {
            let v = iter.next().and_then(|s| s.first().copied()).unwrap_or(0);
            if v == 0 {
                default
            } else {
                v
            }
        };

        match action {
            'A' => {
                let n = p(&mut iter, 1) as usize;
                let row = self.cursor.row.saturating_sub(n);
                self.move_cursor_to(row, self.cursor.col);
            }
            'B' | 'e' => {
                let n = p(&mut iter, 1) as usize;
                let row = (self.cursor.row + n).min(self.rows() - 1);
                self.move_cursor_to(row, self.cursor.col);
            }
            'C' | 'a' => {
                let n = p(&mut iter, 1) as usize;
                let col = (self.cursor.col + n).min(self.cols() - 1);
                self.move_cursor_to(self.cursor.row, col);
            }
            'D' => {
                let n = p(&mut iter, 1) as usize;
                let col = self.cursor.col.saturating_sub(n);
                self.move_cursor_to(self.cursor.row, col);
            }
            'E' => {
                let n = p(&mut iter, 1) as usize;
                let row = (self.cursor.row + n).min(self.rows() - 1);
                self.move_cursor_to(row, 0);
            }
            'F' => {
                let n = p(&mut iter, 1) as usize;
                let row = self.cursor.row.saturating_sub(n);
                self.move_cursor_to(row, 0);
            }
            'G' | '`' => {
                let col = p(&mut iter, 1).saturating_sub(1) as usize;
                self.move_cursor_to(self.cursor.row, col);
            }
            'd' => {
                let row = p(&mut iter, 1).saturating_sub(1) as usize;
                self.move_cursor_to(row, self.cursor.col);
            }
            'H' | 'f' => {
                let row = p(&mut iter, 1).saturating_sub(1) as usize;
                let col = p(&mut iter, 1).saturating_sub(1) as usize;
                self.move_cursor_to(row, col);
            }
            'J' => self.erase_in_display(p(&mut iter, 0)),
            'K' => self.erase_in_line(p(&mut iter, 0)),
            'S' => {
                let n = p(&mut iter, 1) as usize;
                self.scroll_region_up(n);
            }
            'T' => {
                let n = p(&mut iter, 1) as usize;
                self.scroll_region_down(n);
            }
            'm' => self.apply_sgr(params),
            'r' => {
                let top = p(&mut iter, 1).saturating_sub(1) as usize;
                let bottom = p(&mut iter, self.rows() as u16).saturating_sub(1) as usize;
                self.set_scroll_region(top, bottom);
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if intermediates.is_empty() {
            match byte {
                b'c' => self.reset(),
                b'D' => self.index_down(),
                b'M' => self.reverse_index(),
                b'E' => {
                    let row = (self.cursor.row + 1).min(self.rows() - 1);
                    self.move_cursor_to(row, 0);
                }
                b'7' => self.save_cursor(),
                b'8' => self.restore_cursor(),
                _ => {}
            }
        }
    }
}

impl Term {
    fn csi_private_mode(&mut self, params: &Params, action: char) {
        let set = action == 'h';
        for param in params.iter() {
            let Some(&code) = param.first() else { continue };
            match code {
                1 => self.modes.app_cursor_keys = set,
                25 => self.modes.show_cursor = set,
                47 => self.set_alt_screen(set, false),
                1049 => self.set_alt_screen(set, true),
                // Bracketed paste, focus events, sync updates, etc: not
                // implemented in this MVP, but must not panic on receipt.
                _ => {}
            }
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.cursor.fg = Color::Default;
            self.cursor.bg = Color::Default;
            self.cursor.flags = CellFlags::empty();
            return;
        }

        let mut iter = params.iter();
        while let Some(param) = iter.next() {
            let code = param.first().copied().unwrap_or(0);
            match code {
                0 => {
                    self.cursor.fg = Color::Default;
                    self.cursor.bg = Color::Default;
                    self.cursor.flags = CellFlags::empty();
                }
                1 => self.cursor.flags |= CellFlags::BOLD,
                3 => self.cursor.flags |= CellFlags::ITALIC,
                4 => self.cursor.flags |= CellFlags::UNDERLINE,
                7 => self.cursor.flags |= CellFlags::REVERSE,
                22 => self.cursor.flags.remove(CellFlags::BOLD),
                23 => self.cursor.flags.remove(CellFlags::ITALIC),
                24 => self.cursor.flags.remove(CellFlags::UNDERLINE),
                27 => self.cursor.flags.remove(CellFlags::REVERSE),
                30..=37 => self.cursor.fg = Color::Indexed((code - 30) as u8),
                38 => {
                    if let Some(color) = parse_extended_color(param, &mut iter) {
                        self.cursor.fg = color;
                    }
                }
                39 => self.cursor.fg = Color::Default,
                40..=47 => self.cursor.bg = Color::Indexed((code - 40) as u8),
                48 => {
                    if let Some(color) = parse_extended_color(param, &mut iter) {
                        self.cursor.bg = color;
                    }
                }
                49 => self.cursor.bg = Color::Default,
                90..=97 => self.cursor.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.cursor.bg = Color::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
        }
    }

    fn scroll_region_up(&mut self, n: usize) {
        let (top, bottom) = self.scroll_region();
        self.active_grid_mut().scroll_up(top, bottom, n);
    }

    fn scroll_region_down(&mut self, n: usize) {
        let (top, bottom) = self.scroll_region();
        self.active_grid_mut().scroll_down(top, bottom, n);
    }

    fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }

    fn wrap_pending_clear_and_move_left(&mut self) {
        self.wrap_pending = false;
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    fn tab_forward(&mut self) {
        self.wrap_pending = false;
        let next_stop = (self.cursor.col / 8 + 1) * 8;
        self.cursor.col = next_stop.min(self.cols() - 1);
    }
}

/// Parse the color subparameters following a `38` (fg) or `48` (bg) SGR
/// code. Handles both the colon form (`38:2:r:g:b`, all packed into one
/// `Params` slice) and the far more common semicolon form (`38;2;r;g;b`,
/// spread across separate iterator items).
fn parse_extended_color(param: &[u16], iter: &mut ParamsIter) -> Option<Color> {
    if param.len() >= 2 {
        return match param[1] {
            5 if param.len() >= 3 => Some(Color::Indexed(param[2] as u8)),
            2 if param.len() >= 5 => {
                Some(Color::Rgb(param[2] as u8, param[3] as u8, param[4] as u8))
            }
            _ => None,
        };
    }

    match iter.next()?.first().copied()? {
        5 => {
            let n = iter.next()?.first().copied()?;
            Some(Color::Indexed(n as u8))
        }
        2 => {
            let r = iter.next()?.first().copied()?;
            let g = iter.next()?.first().copied()?;
            let b = iter.next()?.first().copied()?;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}
