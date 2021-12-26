use std::error;
use std::ffi;
use std::fmt;
use std::mem;
use std::os::raw;
use std::os::unix::prelude::AsRawFd;
use std::path::Path;
use std::ptr;

use nix::fcntl::{self, OFlag};
use nix::libc;
use nix::sys::select::{self, FdSet};
use nix::unistd::{self, ForkResult};
use nix::{pty, sys::stat};
use x11::xlib;

const SHELL: &str = "/bin/dash";

nix::ioctl_write_ptr_bad!(set_window_size, libc::TIOCSWINSZ, pty::Winsize);
nix::ioctl_none_bad!(set_control_terminal, libc::TIOCSCTTY);

trait Dimention {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

#[derive(Debug, Clone)]
enum Error {
    CantOpenDisplay,
    CantLoadBgColor,
    CantLoadFgColor,
    CantSpawn,
    CantPushElement,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::CantOpenDisplay => write!(f, "Can't open X11 display"),
            Error::CantLoadBgColor => write!(f, "Can't load background color"),
            Error::CantLoadFgColor => write!(f, "Can't load foreground color"),
            Error::CantSpawn => write!(f, "Can't spawn a process"),
            Error::CantPushElement => write!(f, "Can't push element to terminal"),
        }
    }
}

impl error::Error for Error {}

#[derive(Debug)]
struct Pty {
    master: pty::PtyMaster,
    slave: raw::c_int,
}

impl Pty {
    pub fn new() -> Result<Self, Box<dyn error::Error>> {
        let master = pty::posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY)?;
        pty::grantpt(&master)?;
        pty::unlockpt(&master)?;

        let slave_name = unsafe { pty::ptsname(&master)? };
        let slave = fcntl::open(
            Path::new(&slave_name),
            OFlag::O_RDWR | OFlag::O_NOCTTY,
            stat::Mode::empty(),
        )?;

        Ok(Self { master, slave })
    }

    pub fn master(&self) -> raw::c_int {
        self.master.as_raw_fd()
    }

    pub fn slave(&self) -> raw::c_int {
        self.slave
    }
}

#[derive(Debug, Clone)]
struct Term<T> {
    width: u32,
    height: u32,

    x: u32,
    y: u32,

    buffers: Vec<Vec<Option<T>>>,
}

impl<T> Term<T> {
    pub fn new() -> Term<T> {
        Self::default()
    }

    fn x(&self) -> u32 {
        self.x
    }

    fn y(&self) -> u32 {
        self.y
    }

    fn buffers(&self) -> &Vec<Vec<Option<T>>> {
        &self.buffers
    }

    fn carriage_return(&mut self) {
        self.x = 0;
    }

    fn line_feed(&mut self) {
        self.y += 1;

        if self.y >= self.height {
            let _ = self.rotate_buffer(1);

            self.y = self.height.saturating_sub(1);
        }
    }

    fn push_element(&mut self, x: Option<T>) -> Result<(), Box<dyn error::Error>> {
        match self
            .buffers
            .get_mut(self.y as usize)
            .and_then(|row| row.get_mut(self.x as usize))
        {
            Some(e) => {
                *e = x;

                self.x += 1;
                if self.x >= self.width {
                    self.x = 0;
                    self.line_feed();
                }

                Ok(())
            }
            _ => Err(Box::new(Error::CantPushElement)),
        }
    }

    fn rotate_buffer(&mut self, n: usize) -> Result<(), Box<dyn error::Error>> {
        for _ in 0..n {
            self.buffers.remove(0);

            let mut buffer = Vec::new();
            for _ in 0..self.width {
                buffer.push(None);
            }

            self.buffers.push(buffer);
        }

        Ok(())
    }
}

impl<T> Default for Term<T> {
    fn default() -> Self {
        let width = 80;
        let height = 25;

        let mut buffers = Vec::new();
        for _ in 0..height {
            let mut row = Vec::new();

            for _ in 0..width {
                row.push(None);
            }

            buffers.push(row);
        }

        Self {
            width,
            height,
            x: 0,
            y: 0,
            buffers,
        }
    }
}

impl<T> Dimention for Term<T> {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Debug, Clone)]
struct X11<T> {
    display: *mut xlib::Display,
    screen: raw::c_int,
    root: raw::c_ulong,
    fd: raw::c_int,

    font: *mut xlib::XFontStruct,
    font_width: raw::c_int,
    font_height: raw::c_int,

    cmap: raw::c_ulong,
    cell_bg: raw::c_ulong,
    cell_fg: raw::c_ulong,

    term: Term<T>,

    width: raw::c_int,
    height: raw::c_int,

    window: raw::c_ulong,

    gc: xlib::GC,
}

impl<T> X11<T> {
    pub fn new(display_name: *const raw::c_char) -> Result<X11<T>, Box<dyn error::Error>> {
        let display = unsafe { xlib::XOpenDisplay(display_name) };

        if display.is_null() {
            return Err(Box::new(Error::CantOpenDisplay));
        }

        let screen = unsafe { xlib::XDefaultScreen(display) };
        let root = unsafe { xlib::XRootWindow(display, screen) };
        let fd = unsafe { xlib::XConnectionNumber(display) };

        let font_name = ffi::CString::new("fixed")?;
        let font = unsafe { xlib::XLoadQueryFont(display, font_name.as_ptr()) };
        let c = ffi::CString::new("m")?;
        let font_width = unsafe { xlib::XTextWidth(font, c.as_ptr(), 1) };
        let font_height = unsafe { (*font).ascent + (*font).descent };

        let cmap = unsafe { xlib::XDefaultColormap(display, screen) };
        let mut color = unsafe { mem::MaybeUninit::uninit().assume_init() };

        let bg = ffi::CString::new("#000000")?;
        let cell_bg = if unsafe {
            xlib::XAllocNamedColor(display, cmap, bg.as_ptr(), &mut color, &mut color) != 0
        } {
            color.pixel
        } else {
            return Err(Box::new(Error::CantLoadBgColor));
        };

        let fg = ffi::CString::new("#aaaaaa")?;
        let cell_fg = if unsafe {
            xlib::XAllocNamedColor(display, cmap, fg.as_ptr(), &mut color, &mut color) != 0
        } {
            color.pixel
        } else {
            return Err(Box::new(Error::CantLoadFgColor));
        };

        let term = Term::new();

        let width = term.width as i32 * font_width;
        let height = term.height as i32 * font_height;

        let depth = unsafe { xlib::XDefaultDepth(display, screen) };
        let visual = unsafe { xlib::XDefaultVisual(display, screen) };
        let mask = xlib::CWBackPixmap | xlib::CWEventMask;
        let mut attrs: xlib::XSetWindowAttributes =
            unsafe { mem::MaybeUninit::uninit().assume_init() };
        attrs.background_pixmap = xlib::ParentRelative as u64;
        attrs.event_mask = xlib::KeyPressMask | xlib::KeyReleaseMask | xlib::ExposureMask;

        let window = unsafe {
            xlib::XCreateWindow(
                display,
                root,
                0,
                0,
                width as u32,
                height as u32,
                0,
                depth,
                xlib::CopyFromParent as u32,
                visual,
                mask,
                &mut attrs,
            )
        };

        let title = ffi::CString::new("toyterm")?;
        unsafe { xlib::XStoreName(display, window, title.as_ptr()) };
        unsafe { xlib::XMapWindow(display, window) };
        let values = ptr::null_mut();
        let gc = unsafe { xlib::XCreateGC(display, window, 0, values) };

        unsafe { xlib::XSync(display, 0) };

        Ok(Self {
            display,
            screen,
            root,
            fd,
            font,
            font_width,
            font_height,
            cmap,
            cell_bg,
            cell_fg,
            term,
            width,
            height,
            window,
            gc,
        })
    }

    pub fn term_mut(&mut self) -> &mut Term<T> {
        &mut self.term
    }

    pub fn fd(&self) -> raw::c_int {
        self.fd
    }
}

impl X11<char> {
    pub fn redraw(&self) -> Result<(), Box<dyn error::Error>> {
        unsafe { xlib::XSetForeground(self.display, self.gc, self.cell_bg) };
        unsafe {
            xlib::XFillRectangle(
                self.display,
                self.window,
                self.gc,
                0,
                0,
                self.width as u32,
                self.height as u32,
            )
        };

        unsafe { xlib::XSetForeground(self.display, self.gc, self.cell_fg) };
        for (y, row) in self.term.buffers().iter().enumerate() {
            for (x, c) in row.iter().enumerate() {
                let c = match c {
                    Some(c) if !c.is_control() => *c,
                    _ => ' ',
                };

                let buf = ffi::CString::new(c.to_string())?;
                unsafe {
                    xlib::XDrawString(
                        self.display,
                        self.window,
                        self.gc,
                        x as i32 * self.font_width,
                        y as i32 * self.font_height + (*self.font).ascent,
                        buf.as_ptr(),
                        1,
                    )
                };
            }
        }

        unsafe { xlib::XSetForeground(self.display, self.gc, self.cell_fg) };
        unsafe {
            xlib::XFillRectangle(
                self.display,
                self.window,
                self.gc,
                self.term.x() as i32 * self.font_width,
                self.term.y() as i32 * self.font_height,
                self.font_width as u32,
                self.font_height as u32,
            )
        };

        unsafe { xlib::XSync(self.display, 0) };

        Ok(())
    }
}

fn rw_key(event: &mut xlib::XKeyEvent, pty: &Pty) -> Result<(), Box<dyn error::Error>> {
    let mut buf: [raw::c_char; 32] = [0; 32];
    let ksym = ptr::null_mut();

    let num = unsafe {
        xlib::XLookupString(
            &mut *event,
            buf.as_mut_ptr(),
            (mem::size_of::<raw::c_char>() * 32) as i32,
            ksym,
            ptr::null_mut(),
        )
    };

    let mut c = [0; 1];

    for b in buf.iter().take(num as usize) {
        c[0] = *b as u8;
        unistd::write(pty.master(), &c[..])?;
    }

    Ok(())
}

fn set_term_size<T>(x11: &X11<T>, pty: &Pty) -> Result<(), Box<dyn error::Error>> {
    let winsize = pty::Winsize {
        ws_row: x11.term.height() as u16,
        ws_col: x11.term.width() as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe { set_window_size(pty.master(), &winsize)? };

    Ok(())
}

fn spawn(pty: &Pty) -> Result<(), Box<dyn error::Error>> {
    match unsafe { unistd::fork() } {
        Ok(ForkResult::Parent { .. }) => {
            unistd::close(pty.slave())?;
            Ok(())
        }
        Ok(ForkResult::Child { .. }) => {
            unistd::close(pty.master())?;

            unistd::setsid()?;

            unsafe { set_control_terminal(pty.slave())? };

            unistd::dup2(pty.slave(), 0)?;
            unistd::dup2(pty.slave(), 1)?;
            unistd::dup2(pty.slave(), 2)?;
            unistd::close(pty.slave())?;

            let shell = ffi::CString::new(SHELL)?;
            let hyphen = ffi::CString::new("-")?;
            let term = ffi::CString::new("TERM=dumb")?;
            unistd::execve(&shell, &[hyphen], &[term])?;

            Err(Box::new(Error::CantSpawn))
        }
        Err(e) => Err(Box::new(e)),
    }
}

fn run(x11: &mut X11<char>, pty: &Pty) -> Result<(), Box<dyn error::Error>> {
    let mut readable = FdSet::new();
    let mut buf = [0; 1];
    let mut event = unsafe { mem::MaybeUninit::uninit().assume_init() };

    loop {
        readable.clear();
        readable.insert(pty.master());
        readable.insert(x11.fd());

        match select::select(None, &mut readable, None, None, None) {
            Ok(_) if readable.contains(pty.master()) => {
                if unistd::read(pty.master(), &mut buf).is_ok() {
                    match buf[0] {
                        b'\r' => x11.term_mut().carriage_return(),
                        b'\n' => {
                            x11.term_mut().line_feed();
                        }
                        c => {
                            x11.term_mut().push_element(Some(c.into()))?;
                        }
                    }
                } else {
                    return Ok(());
                }
                x11.redraw()?;
            }
            Ok(_) if readable.contains(x11.fd()) => {
                while unsafe { xlib::XPending(x11.display) > 0 } {
                    unsafe { xlib::XNextEvent(x11.display, &mut event) };

                    match unsafe { event.type_ } {
                        xlib::Expose => x11.redraw()?,
                        xlib::KeyPress => rw_key(unsafe { &mut event.key }, pty)?,
                        _ => {}
                    }
                }
            }
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
    }
}

fn main() -> Result<(), Box<dyn error::Error>> {
    println!("Start running toyterm.");

    let mut x11 = X11::new(ptr::null())?;
    println!("Create a connection to X11 server.");

    let pty = Pty::new()?;
    println!("Create a Pty pair.");

    set_term_size(&x11, &pty)?;
    println!("Set pty size.");

    spawn(&pty)?;
    println!("Spawn a shell as a child process.");

    run(&mut x11, &pty)?;

    println!("Exit toyterm.");
    Ok(())
}
