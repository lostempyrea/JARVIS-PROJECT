use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VtColor {
    #[default]
    Default,
    Ansi(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl VtColor {
    pub fn to_css(self, is_fg: bool) -> Option<String> {
        match self {
            VtColor::Default => {
                if is_fg {
                    Some("#D6F4FF".to_string())
                } else {
                    None
                }
            }
            VtColor::Ansi(n) => {
                let code = match n {
                    0 => "#000000",
                    1 => "#800000",
                    2 => "#008000",
                    3 => "#808000",
                    4 => "#3B8EEA",
                    5 => "#800080",
                    6 => "#008080",
                    7 => "#C0C0C0",
                    8 => "#808080",
                    9 => "#FF0000",
                    10 => "#00FF00",
                    11 => "#FFFF00",
                    12 => "#63B3FF",
                    13 => "#FF00FF",
                    14 => "#00FFFF",
                    15 => "#FFFFFF",
                    _ => return VtColor::Indexed(n).to_css(is_fg),
                };
                Some(code.to_string())
            }
            VtColor::Indexed(n) => {
                if n < 16 {
                    VtColor::Ansi(n).to_css(is_fg)
                } else if (16..=231).contains(&n) {
                    let idx = n - 16;
                    let r = idx / 36;
                    let g = (idx % 36) / 6;
                    let b = idx % 6;
                    let to_val = |v: u8| -> u8 {
                        if v == 0 { 0 } else { v * 40 + 55 }
                    };
                    Some(format!("#{:02X}{:02X}{:02X}", to_val(r), to_val(g), to_val(b)))
                } else {
                    let gray = (n - 232) * 10 + 8;
                    Some(format!("#{:02X}{:02X}{:02X}", gray, gray, gray))
                }
            }
            VtColor::Rgb(r, g, b) => Some(format!("#{r:02X}{g:02X}{b:02X}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: VtColor,
    pub bg: VtColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub fg_css: String,
    pub bg_css: Option<String>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub has_cursor: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtScreenSnapshot {
    pub rows: Vec<Vec<StyledSpan>>,
    pub cols: u16,
    pub height: u16,
    pub title: String,
    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    pub scrollback_len: usize,
}

#[derive(Clone)]
struct Cell {
    c: char,
    style: CellStyle,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            style: CellStyle::default(),
        }
    }
}

pub struct VtScreen {
    cols: u16,
    rows: u16,
    grid: Vec<Vec<Cell>>,
    alt_grid: Vec<Vec<Cell>>,
    is_alt: bool,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor_row: usize,
    saved_cursor_col: usize,
    saved_style: CellStyle,
    cursor_visible: bool,
    app_cursor_keys: bool,
    bracketed_paste: bool,
    title: String,
    current_style: CellStyle,
    scrollback: Vec<Vec<Cell>>,
    scroll_top: usize,
    scroll_bottom: usize,
}

impl VtScreen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let grid = vec![vec![Cell::default(); cols as usize]; rows as usize];
        Self {
            cols,
            rows,
            grid: grid.clone(),
            alt_grid: grid,
            is_alt: false,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor_row: 0,
            saved_cursor_col: 0,
            saved_style: CellStyle::default(),
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            title: String::new(),
            current_style: CellStyle::default(),
            scrollback: Vec::new(),
            scroll_top: 0,
            scroll_bottom: (rows as usize).saturating_sub(1),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = (rows as usize).saturating_sub(1);
        for g in [&mut self.grid, &mut self.alt_grid] {
            g.resize(rows as usize, vec![Cell::default(); cols as usize]);
            for row in g.iter_mut() {
                row.resize(cols as usize, Cell::default());
            }
        }
        self.cursor_row = self.cursor_row.min((rows as usize).saturating_sub(1));
        self.cursor_col = self.cursor_col.min((cols as usize).saturating_sub(1));
    }

    pub fn process_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'\r' => {
                    self.cursor_col = 0;
                    i += 1;
                }
                b'\n' => {
                    self.feed_line();
                    i += 1;
                }
                0x08 => {
                    // Backspace
                    if self.cursor_col > 0 {
                        self.cursor_col -= 1;
                    }
                    i += 1;
                }
                b'\t' => {
                    // Tab
                    let target = ((self.cursor_col / 8) + 1) * 8;
                    self.cursor_col = target.min((self.cols as usize).saturating_sub(1));
                    i += 1;
                }
                0x1B => {
                    // ESC
                    if i + 1 < bytes.len() {
                        let next = bytes[i + 1];
                        match next {
                            b'7' => {
                                // DECSC
                                self.saved_cursor_row = self.cursor_row;
                                self.saved_cursor_col = self.cursor_col;
                                self.saved_style = self.current_style;
                                i += 2;
                            }
                            b'8' => {
                                // DECRC
                                self.cursor_row = self.saved_cursor_row.min((self.rows as usize).saturating_sub(1));
                                self.cursor_col = self.saved_cursor_col.min((self.cols as usize).saturating_sub(1));
                                self.current_style = self.saved_style;
                                i += 2;
                            }
                            b'M' => {
                                // RI (Reverse Index)
                                if self.cursor_row == self.scroll_top {
                                    self.scroll_down(self.scroll_top, self.scroll_bottom);
                                } else if self.cursor_row > 0 {
                                    self.cursor_row -= 1;
                                }
                                i += 2;
                            }
                            b'D' => {
                                // IND
                                self.feed_line();
                                i += 2;
                            }
                            b'E' => {
                                // NEL
                                self.feed_line();
                                self.cursor_col = 0;
                                i += 2;
                            }
                            b'c' => {
                                // RIS
                                *self = Self::new(self.cols, self.rows);
                                i += 2;
                            }
                            b'[' => {
                                // CSI
                                let (consumed, reply) = self.parse_csi(&bytes[i + 2..]);
                                replies.extend(reply);
                                i += 2 + consumed;
                            }
                            b']' => {
                                // OSC
                                let (consumed, title_opt) = parse_osc(&bytes[i + 2..]);
                                if let Some(t) = title_opt {
                                    self.title = t;
                                }
                                i += 2 + consumed;
                            }
                            _ => {
                                i += 2;
                            }
                        }
                    } else {
                        i += 1;
                    }
                }
                _ => {
                    // Printable char (UTF-8 simple handling or ascii)
                    // Let's decode UTF-8 point if possible, or just single byte fallback
                    let (ch, len) = decode_utf8(&bytes[i..]);
                    if len == 0 {
                        i += 1;
                    } else {
                        self.put_char(ch);
                        i += len;
                    }
                }
            }
        }
        replies
    }

    fn feed_line(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up(self.scroll_top, self.scroll_bottom);
        } else if self.cursor_row + 1 < self.rows as usize {
            self.cursor_row += 1;
        }
    }

    fn scroll_up(&mut self, top: usize, bottom: usize) {
        if top >= bottom || bottom >= self.rows as usize {
            return;
        }
        let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
        if top == 0 && !self.is_alt {
            self.scrollback.push(grid[0].clone());
            if self.scrollback.len() > 1000 {
                self.scrollback.remove(0);
            }
        }
        for r in top..bottom {
            grid[r] = grid[r + 1].clone();
        }
        grid[bottom] = vec![Cell::default(); self.cols as usize];
    }

    fn scroll_down(&mut self, top: usize, bottom: usize) {
        if top >= bottom || bottom >= self.rows as usize {
            return;
        }
        let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
        for r in (top + 1..=bottom).rev() {
            grid[r] = grid[r - 1].clone();
        }
        grid[top] = vec![Cell::default(); self.cols as usize];
    }

    fn put_char(&mut self, c: char) {
        if self.cursor_col >= self.cols as usize {
            self.cursor_col = 0;
            self.feed_line();
        }
        let r = self.cursor_row;
        let c_col = self.cursor_col;
        if r < self.rows as usize && c_col < self.cols as usize {
            let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
            grid[r][c_col] = Cell {
                c,
                style: self.current_style,
            };
        }
        self.cursor_col += 1;
    }

    fn parse_csi(&mut self, bytes: &[u8]) -> (usize, Vec<u8>) {
        // Find command byte (0x40..=0x7E)
        let mut idx = 0;
        while idx < bytes.len() {
            let b = bytes[idx];
            if (0x40..=0x7E).contains(&b) {
                break;
            }
            idx += 1;
        }
        if idx >= bytes.len() {
            return (bytes.len(), Vec::new());
        }
        let cmd = bytes[idx];
        let param_slice = &bytes[..idx];
        let params = parse_params(param_slice);
        let mut reply = Vec::new();

        let is_private = param_slice.starts_with(b"?");

        match cmd {
            b'A' => {
                // CUU
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            b'B' | b'e' => {
                // CUD / VPR
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_row = (self.cursor_row + n).min((self.rows as usize).saturating_sub(1));
            }
            b'C' | b'a' => {
                // CUF / HPR
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_col = (self.cursor_col + n).min((self.cols as usize).saturating_sub(1));
            }
            b'D' => {
                // CUB
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            b'E' => {
                // CNL
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_row = (self.cursor_row + n).min((self.rows as usize).saturating_sub(1));
                self.cursor_col = 0;
            }
            b'F' => {
                // CPL
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.cursor_col = 0;
            }
            b'G' | b'`' => {
                // CHA / HPA
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_col = (n - 1).min((self.cols as usize).saturating_sub(1));
            }
            b'd' => {
                // VPA
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor_row = (n - 1).min((self.rows as usize).saturating_sub(1));
            }
            b'H' | b'f' => {
                // CUP / HVP
                let r = params.first().copied().unwrap_or(1).max(1) as usize;
                let c = params.get(1).copied().unwrap_or(1).max(1) as usize;
                self.cursor_row = (r - 1).min((self.rows as usize).saturating_sub(1));
                self.cursor_col = (c - 1).min((self.cols as usize).saturating_sub(1));
            }
            b'J' => {
                // ED
                let mode = params.first().copied().unwrap_or(0);
                let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
                match mode {
                    0 => {
                        // cursor to end of screen
                        for c in self.cursor_col..self.cols as usize {
                            if self.cursor_row < grid.len() {
                                grid[self.cursor_row][c] = Cell::default();
                            }
                        }
                        #[allow(clippy::needless_range_loop)]
                        for r in self.cursor_row + 1..grid.len() {
                            grid[r] = vec![Cell::default(); self.cols as usize];
                        }
                    }
                    1 => {
                        // start to cursor
                        for r in 0..self.cursor_row {
                            if r < grid.len() {
                                grid[r] = vec![Cell::default(); self.cols as usize];
                            }
                        }
                        for c in 0..=self.cursor_col.min((self.cols as usize).saturating_sub(1)) {
                            if self.cursor_row < grid.len() {
                                grid[self.cursor_row][c] = Cell::default();
                            }
                        }
                    }
                    2 | 3 => {
                        // entire screen / + scrollback
                        for row in grid.iter_mut() {
                            *row = vec![Cell::default(); self.cols as usize];
                        }
                        if mode == 3 {
                            self.scrollback.clear();
                        }
                    }
                    _ => {}
                }
            }
            b'K' => {
                // EL
                let mode = params.first().copied().unwrap_or(0);
                let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
                if self.cursor_row < grid.len() {
                    let row = &mut grid[self.cursor_row];
                    match mode {
                        0 => {
                            #[allow(clippy::needless_range_loop)]
                            for c in self.cursor_col..row.len() {
                                row[c] = Cell::default();
                            }
                        }
                        1 => {
                            for c in 0..=self.cursor_col.min(row.len().saturating_sub(1)) {
                                row[c] = Cell::default();
                            }
                        }
                        2 => {
                            for cell in row.iter_mut() {
                                *cell = Cell::default();
                            }
                        }
                        _ => {}
                    }
                }
            }
            b'X' => {
                // ECH
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
                if self.cursor_row < grid.len() {
                    let row = &mut grid[self.cursor_row];
                    for c in self.cursor_col..(self.cursor_col + n).min(row.len()) {
                        row[c] = Cell::default();
                    }
                }
            }
            b'@' => {
                // ICH
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
                if self.cursor_row < grid.len() {
                    let row = &mut grid[self.cursor_row];
                    let len = row.len();
                    for c in (self.cursor_col + n..len).rev() {
                        let src = c - n;
                        row[c] = row[src].clone();
                    }
                    #[allow(clippy::needless_range_loop)]
                    for c in self.cursor_col..(self.cursor_col + n).min(len) {
                        row[c] = Cell::default();
                    }
                }
            }
            b'P' => {
                // DCH
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                let grid = if self.is_alt { &mut self.alt_grid } else { &mut self.grid };
                if self.cursor_row < grid.len() {
                    let row = &mut grid[self.cursor_row];
                    let len = row.len();
                    for c in self.cursor_col..len {
                        let src = c + n;
                        row[c] = if src < len { row[src].clone() } else { Cell::default() };
                    }
                }
            }
            b'L' => {
                // IL
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.scroll_down(self.cursor_row, self.scroll_bottom);
                }
            }
            b'M' => {
                // DL
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.scroll_up(self.cursor_row, self.scroll_bottom);
                }
            }
            b'S' => {
                // SU
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.scroll_up(self.scroll_top, self.scroll_bottom);
                }
            }
            b'T' => {
                // SD
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.scroll_down(self.scroll_top, self.scroll_bottom);
                }
            }
            b'r' => {
                // DECSTBM
                let top = params.first().copied().unwrap_or(1).max(1) as usize - 1;
                let bottom = params.get(1).copied().unwrap_or(self.rows).max(1) as usize - 1;
                if top < bottom && bottom < self.rows as usize {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.cursor_row = top;
                    self.cursor_col = 0;
                }
            }
            b's' => {
                // DECSC
                self.saved_cursor_row = self.cursor_row;
                self.saved_cursor_col = self.cursor_col;
                self.saved_style = self.current_style;
            }
            b'u' => {
                // DECRC
                self.cursor_row = self.saved_cursor_row.min((self.rows as usize).saturating_sub(1));
                self.cursor_col = self.saved_cursor_col.min((self.cols as usize).saturating_sub(1));
                self.current_style = self.saved_style;
            }
            b'h' | b'l' => {
                let enable = cmd == b'h';
                if is_private {
                    for p in &params {
                        match p {
                            25 => self.cursor_visible = enable,
                            1 => self.app_cursor_keys = enable,
                            2004 => self.bracketed_paste = enable,
                            47 | 1047 | 1049 => {
                                if enable && !self.is_alt {
                                    self.is_alt = true;
                                    if *p == 1049 {
                                        self.alt_grid = vec![vec![Cell::default(); self.cols as usize]; self.rows as usize];
                                        self.cursor_row = 0;
                                        self.cursor_col = 0;
                                    }
                                } else if !enable && self.is_alt {
                                    self.is_alt = false;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            b'm' => {
                // SGR
                self.parse_sgr(&params);
            }
            b'n' => {
                // DSR
                let mode = params.first().copied().unwrap_or(0);
                if mode == 6 {
                    reply = format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1).into_bytes();
                } else if mode == 5 {
                    reply = b"\x1b[0n".to_vec();
                }
            }
            b'c' => {
                // DA
                reply = b"\x1b[?1;2c".to_vec();
            }
            _ => {}
        }

        (idx + 1, reply)
    }

    fn parse_sgr(&mut self, params: &[u16]) {
        let mut idx = 0;
        while idx < params.len() {
            let p = params[idx];
            match p {
                0 => self.current_style = CellStyle::default(),
                1 => self.current_style.bold = true,
                2 => self.current_style.dim = true,
                3 => self.current_style.italic = true,
                4 => self.current_style.underline = true,
                7 => self.current_style.inverse = true,
                22 => {
                    self.current_style.bold = false;
                    self.current_style.dim = false;
                }
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                27 => self.current_style.inverse = false,
                30..=37 => self.current_style.fg = VtColor::Ansi((p - 30) as u8),
                38 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[idx + 1..]) {
                        self.current_style.fg = color;
                        idx += consumed;
                    }
                }
                39 => self.current_style.fg = VtColor::Default,
                40..=47 => self.current_style.bg = VtColor::Ansi((p - 40) as u8),
                48 => {
                    if let Some((color, consumed)) = parse_extended_color(&params[idx + 1..]) {
                        self.current_style.bg = color;
                        idx += consumed;
                    }
                }
                49 => self.current_style.bg = VtColor::Default,
                90..=97 => self.current_style.fg = VtColor::Ansi((p - 90 + 8) as u8),
                100..=107 => self.current_style.bg = VtColor::Ansi((p - 100 + 8) as u8),
                _ => {}
            }
            idx += 1;
        }
    }

    pub fn snapshot(&self, scroll_offset: usize) -> VtScreenSnapshot {
        let grid = if self.is_alt { &self.alt_grid } else { &self.grid };
        let total_rows = self.scrollback.len() + grid.len();
        let target_start = total_rows.saturating_sub(self.rows as usize + scroll_offset);

        let mut rows = Vec::new();
        for r_idx in 0..(self.rows as usize) {
            let actual_idx = target_start + r_idx;
            let row_cells = if actual_idx < self.scrollback.len() {
                &self.scrollback[actual_idx]
            } else {
                let g_idx = actual_idx - self.scrollback.len();
                if g_idx < grid.len() {
                    &grid[g_idx]
                } else {
                    &grid[grid.len() - 1]
                }
            };

            let is_cursor_row = scroll_offset == 0 && self.cursor_visible && (actual_idx == self.scrollback.len() + self.cursor_row);
            let mut spans = Vec::new();
            let mut current_span: Option<(StyledSpan, CellStyle)> = None;

            for (c_idx, cell) in row_cells.iter().enumerate() {
                let mut style = cell.style;
                if style.inverse {
                    std::mem::swap(&mut style.fg, &mut style.bg);
                }

                let fg_css = style.fg.to_css(true).unwrap_or_else(|| "#D6F4FF".to_string());
                let bg_css = style.bg.to_css(false);

                let has_cursor = if is_cursor_row && c_idx == self.cursor_col { Some(1) } else { None };

                if let Some((ref mut span, ref mut cur_style)) = current_span {
                    if cur_style == &style && span.has_cursor == has_cursor {
                        span.text.push(cell.c);
                        continue;
                    }
                }

                if let Some((span, _)) = current_span.take() {
                    spans.push(span);
                }

                current_span = Some((
                    StyledSpan {
                        text: cell.c.to_string(),
                        fg_css,
                        bg_css,
                        bold: style.bold,
                        dim: style.dim,
                        italic: style.italic,
                        underline: style.underline,
                        has_cursor,
                    },
                    style,
                ));
            }

            if let Some((span, _)) = current_span.take() {
                spans.push(span);
            }

            rows.push(spans);
        }

        VtScreenSnapshot {
            rows,
            cols: self.cols,
            height: self.rows,
            title: self.title.clone(),
            app_cursor_keys: self.app_cursor_keys,
            bracketed_paste: self.bracketed_paste,
            scrollback_len: self.scrollback.len(),
        }
    }
}

fn parse_params(slice: &[u8]) -> Vec<u16> {
    let s = String::from_utf8_lossy(slice);
    s.split(';')
        .filter_map(|part| {
            let clean = part.trim_matches(|c: char| !c.is_ascii_digit());
            clean.parse::<u16>().ok()
        })
        .collect()
}

fn parse_extended_color(params: &[u16]) -> Option<(VtColor, usize)> {
    if params.is_empty() {
        return None;
    }
    if params[0] == 5 && params.len() >= 2 {
        Some((VtColor::Indexed(params[1] as u8), 2))
    } else if params[0] == 2 && params.len() >= 4 {
        Some((VtColor::Rgb(params[1] as u8, params[2] as u8, params[3] as u8), 4))
    } else {
        None
    }
}

fn parse_osc(bytes: &[u8]) -> (usize, Option<String>) {
    let mut end = 0;
    while end < bytes.len() {
        if bytes[end] == 0x07 || (bytes[end] == 0x1B && end + 1 < bytes.len() && bytes[end + 1] == b'\\') {
            break;
        }
        end += 1;
    }
    let slice = &bytes[..end];
    let s = String::from_utf8_lossy(slice);
    let mut parts = s.splitn(2, ';');
    let _code = parts.next();
    let val = parts.next().map(|s| s.to_string());
    let consumed = if end < bytes.len() && bytes[end] == 0x1B { end + 2 } else { end + 1 };
    (consumed, val)
}

fn decode_utf8(bytes: &[u8]) -> (char, usize) {
    if bytes.is_empty() {
        return ('\0', 0);
    }
    let b0 = bytes[0];
    if b0 < 128 {
        return (b0 as char, 1);
    }
    let (width, mask) = if (b0 & 0xE0) == 0xC0 {
        (2, 0x1F)
    } else if (b0 & 0xF0) == 0xE0 {
        (3, 0x0F)
    } else if (b0 & 0xF8) == 0xF0 {
        (4, 0x07)
    } else {
        return ('\0', 1);
    };

    if bytes.len() < width {
        return ('\0', 1);
    }

    let mut val = (b0 & mask) as u32;
    #[allow(clippy::needless_range_loop)]
    for i in 1..width {
        let bi = bytes[i];
        if (bi & 0xC0) != 0x80 {
            return ('\0', 1);
        }
        val = (val << 6) | (bi & 0x3F) as u32;
    }

    let ch = char::from_u32(val).unwrap_or('\0');
    (ch, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_screen_parses_colors_cursor_alt_screen_and_dsr() {
        let mut screen = VtScreen::new(10, 5);

        // Test title OSC
        screen.process_bytes(b"\x1b]0;MyTitle\x07");
        assert_eq!(screen.snapshot(0).title, "MyTitle");

        // Test DSR reply
        let reply = screen.process_bytes(b"\x1b[6n\x1b[5n\x1b[c");
        assert_eq!(reply, b"\x1b[1;1R\x1b[0n\x1b[?1;2c");

        // Test Colors & SGR
        screen.process_bytes(b"\x1b[31mRed\x1b[38;5;208mOrange\x1b[38;2;10;20;30mRGB\x1b[0m");
        let snap = screen.snapshot(0);
        assert_eq!(snap.rows[0][0].text, "Red");
        assert_eq!(snap.rows[0][0].fg_css, "#800000");

        // Test Alt Screen
        screen.process_bytes(b"\x1b[?1049h");
        assert!(screen.is_alt);
        screen.process_bytes(b"\x1b[?1049l");
        assert!(!screen.is_alt);
    }
}
