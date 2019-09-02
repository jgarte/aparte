use bytes::BytesMut;
use std::cell::RefCell;
use std::collections::HashMap;
use std::cmp;
use std::fmt;
use std::hash;
use std::io::{Error as IoError, ErrorKind};
use std::io::{Write, Stdout};
use std::rc::Rc;
use termion::color;
use termion::event::Key;
use termion::input::TermRead;
use termion::cursor::DetectCursorPos;
use termion::raw::{IntoRawMode, RawTerminal};
use termion::screen::AlternateScreen;
use tokio::codec::FramedRead;
use tokio_codec::{Decoder};
use uuid::Uuid;
use xmpp_parsers::{BareJid, Jid};
use chrono::offset::{TimeZone, Local};
use chrono::Utc;

use crate::core::{Plugin, Aparte, Event, Message, XmppMessage, Command, CommandOrMessage, CommandError};

pub type CommandStream = FramedRead<tokio::reactor::PollEvented2<tokio_file_unix::File<std::fs::File>>, KeyCodec>;
type Screen = AlternateScreen<RawTerminal<Stdout>>;

#[derive(Clone)]
enum Dimension {
    MatchParent,
    WrapContent,
    Absolute(u16),
}

trait ViewTrait {
    fn as_view(self: Rc<RefCell<dyn ViewTrait>>) -> Rc<RefCell<dyn ViewTrait>>;
    fn measure(&mut self, width_spec: Option<u16>, height_spec: Option<u16>);
    fn layout(&mut self, top: u16, left: u16);
    fn get_measured_width(&self) -> Option<u16>;
    fn get_measured_height(&self) -> Option<u16>;
    fn redraw(&mut self);
}

#[derive(Clone)]
struct View<T> {
    screen: Rc<RefCell<Screen>>,
    width: Dimension,
    height: Dimension,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    content: T
}

default impl<T> ViewTrait for View<T> {
    fn as_view(self: Rc<RefCell<Self>>) -> Rc<RefCell<dyn ViewTrait>> {
        self
    }

    fn measure(&mut self, width_spec: Option<u16>, height_spec: Option<u16>) {
        self.w = match self.width {
            Dimension::MatchParent => {
                match width_spec {
                    Some(width_spec) => width_spec,
                    None => 0,
                }
            },
            Dimension::WrapContent => unreachable!(),
            Dimension::Absolute(width) => {
                match width_spec {
                    Some(width_spec) => cmp::min(width, width_spec),
                    None => width,
                }
            }
        };

        self.h = match self.height {
            Dimension::MatchParent => {
                match height_spec {
                    Some(height_spec) => height_spec,
                    None => 0,
                }
            },
            Dimension::WrapContent => unreachable!(),
            Dimension::Absolute(height) => {
                match height_spec {
                    Some(height_spec) => cmp::min(height, height_spec),
                    None => height,
                }
            },
        };
    }

    fn layout(&mut self, top: u16, left: u16) {
        self.x = left;
        self.y = top;
    }

    fn get_measured_width(&self) -> Option<u16> {
        if self.w > 0 {
            Some(self.w)
        } else {
            None
        }
    }

    fn get_measured_height(&self) -> Option<u16> {
        if self.h > 0 {
            Some(self.h)
        } else {
            None
        }
    }
}

struct FrameLayout {
    child: Option<Rc<RefCell<dyn ViewTrait>>>,
}

impl View<FrameLayout> {
    fn new(screen: Rc<RefCell<Screen>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            screen: screen,
            width: Dimension::MatchParent,
            height: Dimension::MatchParent,
            x: 1,
            y: 1,
            w: 0,
            h: 0,
            content: FrameLayout {
                child: None,
            }
        }))
    }

    fn set_child(&mut self, child: Rc<RefCell<dyn ViewTrait>>) {
        child.borrow_mut().measure(Some(self.w), Some(self.h));
        child.borrow_mut().layout(self.y, self.x);

        self.content.child = Some(child);
    }
}

impl ViewTrait for View<FrameLayout> {
    fn measure(&mut self, width_spec: Option<u16>, height_spec: Option<u16>) {
        self.w = width_spec.unwrap_or(0);
        self.h = height_spec.unwrap_or(0);

        if let Some(child) = &self.content.child {
            child.borrow_mut().measure(Some(self.w), Some(self.h));
        }
    }

    fn layout(&mut self, top: u16, left: u16) {
        self.x = left;
        self.y = top;

        if let Some(child) = &self.content.child {
            child.borrow_mut().layout(top, left);
        }
    }

    fn redraw(&mut self) {
        if let Some(child) = &self.content.child {
            child.borrow_mut().redraw();
        }
    }
}

#[derive(Clone, PartialEq)]
enum Orientation {
    Vertical,
    Horizontal,
}

#[derive(Clone)]
struct LinearLayout {
    orientation: Orientation,
    children: Vec<Rc<RefCell<dyn ViewTrait>>>,
}

impl View<LinearLayout> {
    fn new(screen: Rc<RefCell<Screen>>, orientation: Orientation, width: Dimension, height: Dimension) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            screen: screen,
            width: width,
            height: height,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            content: LinearLayout {
                orientation: orientation,
                children: Vec::new(),
            }
        }))
    }

    fn push(&mut self, widget: Rc<RefCell<dyn ViewTrait>>) {
        self.content.children.push(widget);
    }
}

impl ViewTrait for View<LinearLayout> {
    fn measure(&mut self, width_spec: Option<u16>, height_spec: Option<u16>) {
        let max_width = match self.width {
            Dimension::MatchParent => width_spec,
            Dimension::WrapContent => width_spec,
            Dimension::Absolute(width) => {
                match width_spec {
                    Some(width_spec) => Some(cmp::min(width, width_spec)),
                    None => Some(width),
                }
            },
        };

        let max_height = match self.height {
            Dimension::MatchParent => height_spec,
            Dimension::WrapContent => height_spec,
            Dimension::Absolute(height) => {
                match height_spec {
                    Some(height_spec) => Some(cmp::min(height, height_spec)),
                    None => Some(height),
                }
            },
        };

        let mut min_width = 0;
        let mut min_height = 0;
        for child in self.content.children.iter_mut() {
            child.borrow_mut().measure(None, None);
            match self.content.orientation {
                Orientation::Vertical => {
                    min_width = cmp::max(min_width, child.borrow().get_measured_width().unwrap_or(0));
                    min_height += child.borrow().get_measured_height().unwrap_or(0);
                },
                Orientation::Horizontal => {
                    min_width += child.borrow().get_measured_height().unwrap_or(0);
                    min_height = cmp::max(min_height, child.borrow().get_measured_height().unwrap_or(0));
                },
            }
        }

        let remaining_width = match max_width {
            Some(max_width) => max_width - min_width,
            None => 0,
        };

        let remaining_height = match max_height {
            Some(max_height) => max_height - min_height,
            None => 0,
        };

        // Split remaining space to children that don't know their size
        let splitted_width = match self.content.orientation {
            Orientation::Vertical => max_width,
            Orientation::Horizontal => {
                let unsized_children = self.content.children.iter().filter(|child| child.borrow().get_measured_width().is_none());
                Some(match unsized_children.collect::<Vec<_>>().len() {
                    0 => 0,
                    count => remaining_width / count as u16,
                })
            },
        };
        let splitted_height = match self.content.orientation {
            Orientation::Vertical => {
                let unsized_children = self.content.children.iter().filter(|child| child.borrow().get_measured_height().is_none());
                Some(match unsized_children.collect::<Vec<_>>().len() {
                    0 => 0,
                    count => remaining_height / count as u16,
                })
            },
            Orientation::Horizontal => max_height,
        };

        self.w = 0;
        self.h = 0;

        for child in self.content.children.iter_mut() {
            let mut width_spec = match child.borrow().get_measured_width() {
                Some(w) => Some(w),
                None => splitted_width,
            };

            let mut height_spec = match child.borrow().get_measured_height() {
                Some(h) => Some(h),
                None => splitted_height,
            };

            if self.content.orientation == Orientation::Horizontal && max_width.is_some() {
               width_spec = Some(cmp::min(width_spec.unwrap(), max_width.unwrap() - self.w));
            }

            if self.content.orientation == Orientation::Vertical && max_height.is_some() {
                height_spec = Some(cmp::min(height_spec.unwrap(), max_height.unwrap() - self.h));
            }

            child.borrow_mut().measure(width_spec, height_spec);

            match self.content.orientation {
                Orientation::Vertical => {
                    self.w = cmp::max(self.w, child.borrow().get_measured_width().unwrap());
                    self.h += child.borrow().get_measured_height().unwrap();
                },
                Orientation::Horizontal => {
                    self.w += child.borrow().get_measured_width().unwrap();
                    self.h = cmp::max(self.w, child.borrow().get_measured_height().unwrap());
                },
            }
        }
    }

    fn layout(&mut self, top: u16, left: u16) {
        self.x = left;
        self.y = top;

        let mut x = self.x;
        let mut y = self.y;

        for child in self.content.children.iter_mut() {
            child.borrow_mut().layout(y, x);
            match self.content.orientation {
                Orientation::Vertical => y += child.borrow_mut().get_measured_height().unwrap(),
                Orientation::Horizontal => x += child.borrow_mut().get_measured_width().unwrap(),
            }
        }
    }

    fn redraw(&mut self) {
        for child in self.content.children.iter_mut() {
            child.borrow_mut().redraw();
        }
    }
}

struct Input {
    buf: String,
    tmp_buf: Option<String>,
    password: bool,
    history: Vec<String>,
    history_index: usize,
}

impl View<Input> {
    fn new(screen: Rc<RefCell<Screen>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            screen: screen,
            width: Dimension::MatchParent,
            height: Dimension::Absolute(1),
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            content: Input {
                buf: String::new(),
                tmp_buf: None,
                password: false,
                history: Vec::new(),
                history_index: 0,
            }
        }))
    }

    fn key(&mut self, c: char) {
        let mut screen = self.screen.borrow_mut();
        self.content.buf.push(c);
        if !self.content.password {
            write!(screen, "{}", c).unwrap();
            screen.flush().unwrap();
        }
    }

    fn delete(&mut self) {
        let mut screen = self.screen.borrow_mut();
        self.content.buf.pop();
        if !self.content.password {
            write!(screen, "{} {}", termion::cursor::Left(1), termion::cursor::Left(1)).unwrap();
            screen.flush().unwrap();
        }
    }

    fn clear(&mut self) {
        let mut screen = self.screen.borrow_mut();
        self.content.buf.clear();
        let _ = self.content.tmp_buf.take();
        self.content.password = false;
        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        for _ in 0 .. self.w {
            write!(screen, " ").unwrap();
        }
        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        screen.flush().unwrap();
    }

    fn left(&mut self) {
        if !self.content.password {
            let mut screen = self.screen.borrow_mut();
            write!(screen, "{}", termion::cursor::Left(1)).unwrap();
            screen.flush().unwrap();
        }
    }

    fn right(&mut self) {
        if !self.content.password {
            let mut screen = self.screen.borrow_mut();
            let (x, _y) = screen.cursor_pos().unwrap();
            if x as usize <= self.content.buf.len() {
                write!(screen, "{}", termion::cursor::Right(1)).unwrap();
                screen.flush().unwrap();
            }
        }
    }

    fn password(&mut self) {
        self.clear();
        self.content.password = true;
        let mut screen = self.screen.borrow_mut();
        write!(screen, "password: ").unwrap();
        screen.flush().unwrap();
    }

    fn validate(&mut self) {
        if !self.content.password {
            self.content.history.push(self.content.buf.clone());
            self.content.history_index = self.content.history.len();
        }
        self.clear();
    }

    fn previous(&mut self) {
        if self.content.history_index == 0 {
            return;
        }

        if self.content.tmp_buf.is_none() {
            self.content.tmp_buf = Some(self.content.buf.clone());
        }

        self.content.history_index -= 1;
        self.content.buf = self.content.history[self.content.history_index].clone();
        self.redraw();
    }

    fn next(&mut self) {
        if self.content.history_index == self.content.history.len() {
            return;
        }

        self.content.history_index += 1;
        if self.content.history_index == self.content.history.len() {
            self.content.buf = self.content.tmp_buf.take().unwrap();
        } else {
            self.content.buf = self.content.history[self.content.history_index].clone();
        }

        self.redraw();
    }
}

impl ViewTrait for View<Input> {
    fn redraw(&mut self) {
        let mut screen = self.screen.borrow_mut();

        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        for _ in 0 .. self.w {
            write!(screen, " ").unwrap();
        }
        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        write!(screen, "{}", self.content.buf).unwrap();

        screen.flush().unwrap();
    }
}

struct TitleBar {
    window_name: Option<String>,
}

impl View<TitleBar> {
    fn new(screen: Rc<RefCell<Screen>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            screen: screen,
            width: Dimension::MatchParent,
            height: Dimension::Absolute(1),
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            content: TitleBar {
                window_name: None,
            },
        }))
    }

    fn set_name(&mut self, name: &str) {
        self.content.window_name = Some(name.to_string());
        self.redraw();
    }
}

impl ViewTrait for View<TitleBar> {
    fn redraw(&mut self) {
        let mut screen = self.screen.borrow_mut();

        write!(screen, "{}", termion::cursor::Save).unwrap();
        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        write!(screen, "{}{}", color::Bg(color::Blue), color::Fg(color::White)).unwrap();

        for _ in 0 .. self.w {
            write!(screen, " ").unwrap();
        }
        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        if let Some(window_name) = &self.content.window_name {
            write!(screen, " {}", window_name).unwrap();
        }

        write!(screen, "{}{}", color::Bg(color::Reset), color::Fg(color::Reset)).unwrap();
        write!(screen, "{}", termion::cursor::Restore).unwrap();
        screen.flush().unwrap();
    }
}

struct WinBar {
    connection: Option<String>,
    windows: Vec<String>,
    current_window: Option<String>,
    highlighted: Vec<String>,
}

impl View<WinBar> {
    fn new(screen: Rc<RefCell<Screen>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            screen: screen,
            width: Dimension::MatchParent,
            height: Dimension::Absolute(1),
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            content: WinBar {
                connection: None,
                windows: Vec::new(),
                current_window: None,
                highlighted: Vec::new(),
            }
        }))

    }

    fn add_window(&mut self, window: &str) {
        self.content.windows.push(window.to_string());
        self.redraw();
    }

    fn set_current_window(&mut self, window: &str) {
        self.content.current_window = Some(window.to_string());
        self.content.highlighted.drain_filter(|w| w == &window);
        self.redraw();
    }

    fn highlight_window(&mut self, window: &str) {
        if self.content.highlighted.iter().find(|w| w == &window).is_none() {
            self.content.highlighted.push(window.to_string());
            self.redraw();
        }
    }
}

impl ViewTrait for View<WinBar> {
    fn redraw(&mut self) {
        let mut screen = self.screen.borrow_mut();

        write!(screen, "{}", termion::cursor::Save).unwrap();
        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        write!(screen, "{}{}", color::Bg(color::Blue), color::Fg(color::White)).unwrap();

        for _ in 0 .. self.w {
            write!(screen, " ").unwrap();
        }

        write!(screen, "{}", termion::cursor::Goto(self.x, self.y)).unwrap();
        if let Some(connection) = &self.content.connection {
            write!(screen, " {}", connection).unwrap();
        }

        let mut windows = String::new();
        let mut windows_len = 0;

        let mut index = 1;
        for window in &self.content.windows {
            if let Some(current) = &self.content.current_window {
                if window == current {
                    let win = format!("-{}: {}- ", index, window);
                    windows_len += win.len();
                    windows.push_str(&win);
                } else {
                    if self.content.highlighted.iter().find(|w| w == &window).is_some() {
                        windows.push_str(&format!("{}", termion::style::Bold));
                    }
                    let win = format!("[{}: {}] ", index, window);
                    windows_len += win.len();
                    windows.push_str(&win);
                    windows.push_str(&format!("{}", termion::style::NoBold));
                }
            }
            index += 1;
        }

        let start = self.x + self.w - windows_len as u16;
        write!(screen, "{}{}", termion::cursor::Goto(start, self.y), windows).unwrap();

        write!(screen, "{}{}", color::Bg(color::Reset), color::Fg(color::Reset)).unwrap();
        write!(screen, "{}", termion::cursor::Restore).unwrap();
        screen.flush().unwrap();
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::Log(message) => {
                let timestamp = Local.from_utc_datetime(&message.timestamp.naive_local());
                write!(f, "{} - {}", timestamp.format("%T"), message.body)
            },
            Message::Incoming(XmppMessage::Chat(message)) => {
                let timestamp = Local.from_utc_datetime(&message.timestamp.naive_local());
                let padding_len = format!("{} - {}: ", timestamp.format("%T"), message.from).len();
                let padding = " ".repeat(padding_len);

                write!(f, "{} - {}{}:{} ", timestamp.format("%T"), color::Fg(color::Green), message.from, color::Fg(color::White))?;

                let mut iter = message.body.lines();
                if let Some(line) = iter.next() {
                    write!(f, "{}", line)?;
                }
                while let Some(line) = iter.next() {
                    write!(f, "\n{}{}", padding, line)?;
                }

                Ok(())
            },
            Message::Outgoing(XmppMessage::Chat(message)) => {
                let timestamp = Local.from_utc_datetime(&message.timestamp.naive_local());
                write!(f, "{} - {}me:{} {}", timestamp.format("%T"), color::Fg(color::Yellow), color::Fg(color::White), message.body)
            }
            Message::Incoming(XmppMessage::Groupchat(message)) => {
                if let Jid::Full(from) = &message.from_full {
                    let timestamp = Local.from_utc_datetime(&message.timestamp.naive_local());
                    let padding_len = format!("{} - {}: ", timestamp.format("%T"), from.resource).len();
                    let padding = " ".repeat(padding_len);

                    write!(f, "{} - {}{}:{} ", timestamp.format("%T"), color::Fg(color::Green), from.resource, color::Fg(color::White))?;

                    let mut iter = message.body.lines();
                    if let Some(line) = iter.next() {
                        write!(f, "{}", line)?;
                    }
                    while let Some(line) = iter.next() {
                        write!(f, "\n{}{}", padding, line)?;
                    }
                }
                Ok(())
            },
            Message::Outgoing(XmppMessage::Groupchat(message)) => {
                let timestamp = Local.from_utc_datetime(&message.timestamp.naive_local());
                write!(f, "{} - {}me:{} {}", timestamp.format("%T"), color::Fg(color::Yellow), color::Fg(color::White), message.body)
            }
        }
    }
}

trait BufferedMessage = fmt::Display + hash::Hash + std::cmp::Eq + std::clone::Clone;

trait Window<T: BufferedMessage>: ViewTrait {
    fn recv_message(&mut self, message: &T, print: bool);
    fn send_message(&self);
    fn page_up(&mut self);
    fn page_down(&mut self);
}

struct BufferedWin<T: BufferedMessage> {
    next_line: u16,
    buf: Vec<T>,
    history: HashMap<T, usize>,
    view: usize,
}

impl<T: BufferedMessage> View<BufferedWin<T>> {
    fn new(screen: Rc<RefCell<Screen>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            screen: screen,
            width: Dimension::MatchParent,
            height: Dimension::MatchParent,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            content: BufferedWin {
                next_line: 0,
                buf: Vec::new(),
                history: HashMap::new(),
                view: 0,
            }
        }))
    }
}

impl<T: BufferedMessage> Window<T> for View<BufferedWin<T>> {
    fn recv_message(&mut self, message: &T, print: bool) {
        if self.content.history.contains_key(message) {
            return;
        }

        self.content.history.insert(message.clone(), self.content.buf.len());
        self.content.buf.push(message.clone());

        if print {
            self.redraw();
        }
    }

    fn page_up(&mut self) {
        let buffers = self.content.buf.iter().flat_map(|m| format!("{}", m).lines().map(str::to_owned).collect::<Vec<_>>());
        let count = buffers.collect::<Vec<_>>().len();

        if count < self.h as usize {
            return;
        }

        let max = count - self.h as usize;

        if self.content.view + (self.h as usize) < max {
            self.content.view += self.h as usize;
        } else {
            self.content.view = max;
        }

        self.redraw();
    }

    fn page_down(&mut self) {
        if self.content.view > self.h as usize {
            self.content.view -= self.h as usize;
        } else {
            self.content.view = 0;
        }
        self.redraw();
    }

    fn send_message(&self) {
    }
}

impl<T: BufferedMessage> ViewTrait for View<BufferedWin<T>> {
    fn redraw(&mut self) {
        let mut screen = self.screen.borrow_mut();

        write!(screen, "{}", termion::cursor::Save).unwrap();

        self.content.next_line = 0;
        let buffers = self.content.buf.iter().flat_map(|m| format!("{}", m).lines().map(str::to_owned).collect::<Vec<_>>());
        let count = buffers.collect::<Vec<_>>().len();

        let mut buffers = self.content.buf.iter().flat_map(|m| format!("{}", m).lines().map(str::to_owned).collect::<Vec<_>>());

        if count > self.h as usize {
            for _ in 0 .. count - self.h as usize - self.content.view {
                if buffers.next().is_none() {
                    break;
                }
            }
        }

        for y in self.y .. self.y + self.h {
            write!(screen, "{}", termion::cursor::Goto(self.x, y)).unwrap();

            for _ in self.x  .. self.x + self.w {
                write!(screen, " ").unwrap();
            }

            write!(screen, "{}", termion::cursor::Goto(self.x, y)).unwrap();

            if let Some(buf) = buffers.next() {
                write!(screen, "{}", buf).unwrap();
                self.content.next_line += 1;
            }
            screen.flush().unwrap();
        }

        write!(screen, "{}", termion::cursor::Restore).unwrap();

        screen.flush().unwrap();
    }
}

pub struct ChatWin {
    us: BareJid,
    them: BareJid,
}

pub struct GroupchatWin {
    us: BareJid,
    groupchat: BareJid,
}

    //fn chat(screen: Rc<RefCell<Screen>>, us: &BareJid, them: &BareJid) -> Self {
    //    let bufwin = Self::bufwin::<Message>(screen);

    //    Window::Chat(ChatWin {
    //        bufwin: bufwin,
    //        us: us.clone(),
    //        them: them.clone(),
    //    })
    //}

    //fn groupchat(screen: Rc<RefCell<Screen>>, us: &BareJid, groupchat: &BareJid) -> Self {
    //    let bufwin = Self::bufwin::<Message>(screen);

    //    Window::Groupchat(GroupchatWin {
    //        bufwin: bufwin,
    //        us: us.clone(),
    //        groupchat: groupchat.clone(),
    //    })
    //}

pub struct UIPlugin {
    screen: Rc<RefCell<Screen>>,
    root: Rc<RefCell<dyn ViewTrait>>,
    frame: Rc<RefCell<View<FrameLayout>>>,
    input: Rc<RefCell<View<Input>>>,
    title_bar: Rc<RefCell<View<TitleBar>>>,
    win_bar: Rc<RefCell<View<WinBar>>>,
    windows: HashMap<String, Rc<RefCell<dyn Window<Message>>>>,
    windows_index: Vec<String>,
    current: String,
    password_command: Option<Command>,
}

impl UIPlugin {
    pub fn command_stream(&self, aparte: Rc<Aparte>) -> CommandStream {
        let file = tokio_file_unix::raw_stdin().unwrap();
        let file = tokio_file_unix::File::new_nb(file).unwrap();
        let file = file.into_io(&tokio::reactor::Handle::default()).unwrap();

        FramedRead::new(file, KeyCodec::new(aparte))
    }

    pub fn current_window(&mut self) -> Rc<RefCell<dyn Window<Message>>> {
        self.windows.get(&self.current).unwrap().clone()
    }

    pub fn switch(&mut self, chat: &str) -> Result<(), ()> {
        let mut title_bar = self.title_bar.borrow_mut();
        let mut win_bar = self.win_bar.borrow_mut();
        let mut frame = self.frame.borrow_mut();
        self.current = chat.to_string();
        if let Some(chat) = self.windows.get(chat) {
            frame.set_child(chat.clone().as_view());
            title_bar.set_name(&self.current);
            win_bar.set_current_window(&self.current);
            frame.redraw();
            return Ok(())
        } else {
            return Err(())
        }
    }

    fn add_window(&mut self, name: &str, window: Rc<RefCell<dyn Window<Message>>>) {
        self.windows.insert(name.to_string(), window);
        self.windows_index.push(name.to_string());
        self.win_bar.borrow_mut().add_window(name);
    }

    pub fn next_window(&mut self) -> Result<(), ()> {
        let index = self.windows_index.iter().position(|name| name == &self.current).unwrap();
        if index + 1 < self.windows_index.len() {
            let name = self.windows_index[index + 1].clone();
            self.switch(&name)
        } else {
            Err(())
        }
    }

    pub fn prev_window(&mut self) -> Result<(), ()> {
        let index = self.windows_index.iter().position(|name| name == &self.current).unwrap();
        if index > 0 {
            let name = self.windows_index[index - 1].clone();
            self.switch(&name)
        } else {
            Err(())
        }
    }

    pub fn read_password(&mut self, command: Command) {
        self.password_command = Some(command);
        self.input.borrow_mut().password();
    }
}

impl Plugin for UIPlugin {
    fn new() -> Self {
        let stdout = std::io::stdout().into_raw_mode().unwrap();
        let screen = Rc::new(RefCell::new(AlternateScreen::from(stdout)));
        let root = View::<LinearLayout>::new(screen.clone(), Orientation::Vertical, Dimension::MatchParent, Dimension::MatchParent);

        let title_bar = View::<TitleBar>::new(screen.clone());
        root.borrow_mut().push(title_bar.clone() as Rc<RefCell<dyn ViewTrait>>);
        let frame = View::<FrameLayout>::new(screen.clone());
        root.borrow_mut().push(frame.clone() as Rc<RefCell<dyn ViewTrait>>);
        let win_bar = View::<WinBar>::new(screen.clone());
        root.borrow_mut().push(win_bar.clone() as Rc<RefCell<dyn ViewTrait>>);
        let input = View::<Input>::new(screen.clone());
        root.borrow_mut().push(input.clone() as Rc<RefCell<dyn ViewTrait>>);

        Self {
            screen: screen,
            root: root as Rc<RefCell<dyn ViewTrait>>,
            frame: frame,
            input: input,
            title_bar: title_bar,
            win_bar: win_bar,
            windows: HashMap::new(),
            windows_index: Vec::new(),
            current: "console".to_string(),
            password_command: None,
        }
    }

    fn init(&mut self, _aparte: &Aparte) -> Result<(), ()> {
        {
            let mut screen = self.screen.borrow_mut();
            write!(screen, "{}", termion::clear::All).unwrap();
        }

        let (width, height) = termion::terminal_size().unwrap();
        self.root.borrow_mut().measure(Some(width), Some(height));
        self.root.borrow_mut().layout(1, 1);

        let console = View::<BufferedWin<Message>>::new(self.screen.clone());
        self.add_window("console", console as Rc<RefCell<dyn Window<Message>>>);
        self.title_bar.borrow_mut().set_name("console");

        self.input.borrow_mut().redraw();
        self.title_bar.borrow_mut().redraw();
        self.win_bar.borrow_mut().redraw();
        self.switch("console").unwrap();

        Ok(())
    }

    fn on_event(&mut self, aparte: Rc<Aparte>, event: &Event) {
        match event {
            Event::Connected(_jid) => {
                self.win_bar.borrow_mut().content.connection = match aparte.current_connection() {
                    Some(jid) => Some(jid.to_string()),
                    None => None,
                };
                self.win_bar.borrow_mut().redraw();
            },
            Event::Message(message) => {
                let chat_name = match message {
                    Message::Incoming(XmppMessage::Chat(message)) => message.from.to_string(),
                    Message::Outgoing(XmppMessage::Chat(message)) => message.to.to_string(),
                    Message::Incoming(XmppMessage::Groupchat(message)) => message.from.to_string(),
                    Message::Outgoing(XmppMessage::Groupchat(message)) => message.to.to_string(),
                    Message::Log(_message) => "console".to_string(),
                };

                let chat = match self.windows.get_mut(&chat_name) {
                    Some(chat) => chat,
                    None => {
                        let mut chat: Rc<RefCell<dyn Window<Message>>> = match message {
                            //Message::Incoming(XmppMessage::Chat(message)) => Window::chat(self.screen.clone(), &message.to, &message.from),
                            //Message::Outgoing(XmppMessage::Chat(message)) => Window::chat(self.screen.clone(), &message.from, &message.to),
                            //Message::Incoming(XmppMessage::Groupchat(message)) => Window::groupchat(self.screen.clone(), &message.to, &message.from),
                            //Message::Outgoing(XmppMessage::Groupchat(message)) => Window::groupchat(self.screen.clone(), &message.from, &message.to),
                            Message::Log(_) => unreachable!(),
                            _ => unreachable!(),
                        };
                        chat.borrow_mut().redraw();
                        self.add_window(&chat_name, chat);
                        self.windows.get_mut(&chat_name).unwrap()
                    },
                };

                chat.borrow_mut().recv_message(message, self.current == chat_name);
                if self.current != chat_name {
                    self.win_bar.borrow_mut().highlight_window(&chat_name);
                }
            },
            Event::Chat(jid) => {
                let chat_name = jid.to_string();
                if self.switch(&chat_name).is_err() {
                    //let us = aparte.current_connection().unwrap().clone().into();
                    //let mut chat = Window::chat(self.screen.clone(), &us, jid);
                    //chat.redraw();
                    //self.add_window(&chat_name, chat);
                    //self.switch(&chat_name).unwrap();
                }
            },
            Event::Join(jid) => {
                let groupchat: BareJid = jid.clone().into();
                let win_name = groupchat.to_string();
                if self.switch(&win_name).is_err() {
                    //let us = aparte.current_connection().unwrap().clone().into();
                    //let groupchat = jid.clone().into();
                    //let chat = Window::groupchat(self.screen.clone(), &us, &groupchat);
                    //self.add_window(&win_name, chat);
                    //self.switch(&win_name).unwrap();
                }
            }
            _ => {},
        }
    }
}

impl fmt::Display for UIPlugin {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Aparté UI")
    }
}

pub struct KeyCodec {
    queue: Vec<Result<CommandOrMessage, CommandError>>,
    aparte: Rc<Aparte>,
}

impl KeyCodec {
    pub fn new(aparte: Rc<Aparte>) -> Self {
        Self {
            queue: Vec::new(),
            aparte: aparte,
        }
    }
}

impl Decoder for KeyCodec {
    type Item = CommandOrMessage;
    type Error = CommandError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut ui = self.aparte.get_plugin_mut::<UIPlugin>().unwrap();

        let mut keys = buf.keys();
        while let Some(key) = keys.next() {
            match key {
                Ok(Key::Backspace) => {
                    ui.input.borrow_mut().delete();
                },
                Ok(Key::Left) => {
                    ui.input.borrow_mut().left();
                },
                Ok(Key::Right) => {
                    ui.input.borrow_mut().right();
                },
                Ok(Key::Up) => {
                    ui.input.borrow_mut().previous();
                },
                Ok(Key::Down) => {
                    ui.input.borrow_mut().next();
                },
                Ok(Key::PageUp) => {
                    ui.current_window().borrow_mut().page_up();
                },
                Ok(Key::PageDown) => {
                    ui.current_window().borrow_mut().page_down();
                },
                Ok(Key::Char('\n')) => {
                    if ui.input.borrow_mut().content.password {
                        let mut command = ui.password_command.take().unwrap();
                        command.args.push(ui.input.borrow_mut().content.buf.clone());
                        self.queue.push(Ok(CommandOrMessage::Command(command)));
                    } else if ui.input.borrow_mut().content.buf.starts_with("/") {
                        let splitted = shell_words::split(&ui.input.borrow_mut().content.buf);
                        match splitted {
                            Ok(splitted) => {
                                let command = Command::new(splitted[0][1..].to_string(), splitted[1..].to_vec());
                                self.queue.push(Ok(CommandOrMessage::Command(command)));
                            },
                            Err(err) => self.queue.push(Err(CommandError::Parse(err))),
                        }
                    } else if ui.input.borrow_mut().content.buf.len() > 0 {
                        //match ui.current_window() {
                        //    Window::Chat(chat) => {
                        //        let from: Jid = chat.us.clone().into();
                        //        let to: Jid = chat.them.clone().into();
                        //        let id = Uuid::new_v4();
                        //        let timestamp = Utc::now();
                        //        let message = Message::outgoing_chat(id.to_string(), timestamp, &from, &to, &ui.input.borrow_mut().content.buf);
                        //        self.queue.push(Ok(CommandOrMessage::Message(message)));
                        //    },
                        //    Window::Groupchat(groupchat) => {
                        //        let from: Jid = groupchat.us.clone().into();
                        //        let to: Jid = groupchat.groupchat.clone().into();
                        //        let id = Uuid::new_v4();
                        //        let timestamp = Utc::now();
                        //        let message = Message::outgoing_groupchat(id.to_string(), timestamp, &from, &to, &ui.input.borrow_mut().content.buf);
                        //        self.queue.push(Ok(CommandOrMessage::Message(message)));
                        //    },
                        //}
                    }
                    if ui.input.borrow_mut().content.buf.len() > 0 {
                        ui.input.borrow_mut().validate();
                    }
                },
                Ok(Key::Alt('\x1b')) => {
                    match keys.next() {
                        Some(Ok(Key::Char('['))) => {
                            match keys.next() {
                                Some(Ok(Key::Char('C'))) => {
                                    let _ = ui.next_window();
                                },
                                Some(Ok(Key::Char('D'))) => {
                                    let _ = ui.prev_window();
                                },
                                Some(Ok(_)) => {},
                                Some(Err(_)) => {},
                                None => {},
                            };
                        },
                        Some(Ok(_)) => {},
                        Some(Err(_)) => {},
                        None => {},
                    };
                },
                Ok(Key::Char(c)) => {
                    ui.input.borrow_mut().key(c);
                },
                Ok(Key::Ctrl('c')) => {
                    self.queue.push(Err(CommandError::Io(IoError::new(ErrorKind::BrokenPipe, "ctrl+c"))));
                },
                Ok(_) => {},
                Err(_) => {},
            };
        }

        buf.clear();

        match self.queue.pop() {
            Some(Ok(command)) => Ok(Some(command)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}
