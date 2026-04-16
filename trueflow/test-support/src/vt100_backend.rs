use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

#[derive(Clone)]
struct SharedParser {
    parser: Rc<RefCell<vt100::Parser>>,
}

impl Write for SharedParser {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.parser.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.parser.borrow_mut().flush()
    }
}

pub struct VT100Backend {
    parser: Rc<RefCell<vt100::Parser>>,
    backend: CrosstermBackend<SharedParser>,
}

impl VT100Backend {
    pub fn new(width: u16, height: u16) -> Self {
        crossterm::style::force_color_output(true);
        let parser = Rc::new(RefCell::new(vt100::Parser::new(height, width, 0)));
        let backend = CrosstermBackend::new(SharedParser {
            parser: Rc::clone(&parser),
        });
        Self { parser, backend }
    }

    pub fn screen_contents(&self) -> String {
        self.parser.borrow().screen().contents()
    }

    pub fn rows(&self) -> Vec<String> {
        self.screen_contents()
            .lines()
            .map(ToString::to_string)
            .collect()
    }
}

impl Backend for VT100Backend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.backend.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let (row, col) = self.parser.borrow().screen().cursor_position();
        Ok(Position::new(col, row))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.backend.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.backend.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.backend.clear_region(clear_type)
    }

    fn append_lines(&mut self, line_count: u16) -> io::Result<()> {
        self.backend.append_lines(line_count)
    }

    fn size(&self) -> io::Result<Size> {
        let (rows, cols) = self.parser.borrow().screen().size();
        Ok(Size::new(cols, rows))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let (rows, cols) = self.parser.borrow().screen().size();
        Ok(WindowSize {
            columns_rows: Size::new(cols, rows),
            pixels: Size::new(640, 480),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.backend)
    }
}
