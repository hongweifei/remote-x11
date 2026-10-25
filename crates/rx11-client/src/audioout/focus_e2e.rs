//! The client's focus attribution, against a REAL X server.
//!
//! The unit tests pin the attribution rule with made-up ids. This test
//! drives the whole chain against the X server on this machine:
//!
//! 1. a real X connection is opened — the same setup request the client
//!    forwards for an application — and the setup reply's resource range
//!    is read: the range that connection owns;
//! 2. a window is created ON THAT CONNECTION, so the window's id comes
//!    from that range;
//! 3. the focus is set to that window;
//! 4. the client asks the server for the focus, through the very
//!    [`ResidentMonitor`] the serve loop uses, and looks the id up in
//!    the ranges: the connection that created the window must come back.
//!
//! This is the missing link of the whole feature: the rule "the channel
//! whose tag is the foreground connection is the one that is heard" is
//! only as good as the foreground's correctness.
//!
//! It needs an X server to talk to. The test is IGNORED by default and
//! runs when pointed at a display:
//!
//! ```text
//! cargo test -p rx11-client --lib -- --ignored
//! ```
//!
//! The display comes from the `RX11_TEST_X11` variable (a host:port
//! pair, the way the client is configured). Without one the test is
//! skipped rather than failed: the machine may have no X server at all.

use rx11_core::types::ConnectionId;

use super::focus::{foreground_connection, ResourceIndex, ResourceRange};
use super::monitor::ResidentMonitor;
use super::x11wire::{Authorization, SetupRequestWatcher};

/// The X server the test talks to.
fn display_address() -> Option<String> {
    let address = std::env::var("RX11_TEST_X11").ok()?;
    if address.is_empty() {
        return None;
    }
    Some(address)
}

/// The authorization the test presents.
///
/// The test must not read the user's `~/.Xauthority` behind their back,
/// so it presents a cookie of its own. The local X server accepts
/// whatever it is given, so the cookie's value does not matter — but it
/// must be PRESENT, because the client only asks the server about the
/// focus once it has an authorization.
fn test_authorization() -> Authorization {
    Authorization {
        name: b"RX11-TEST-COOKIE".to_vec(),
        data: vec![0x5a; 16],
    }
}

/// The setup request for a connection.
fn setup_request() -> Vec<u8> {
    let authorization = test_authorization();
    let (_connect, request) = x11rb_protocol::connect::Connect::with_authorization(
        authorization.name,
        authorization.data,
    );
    request
}

/// Read the resource range the server granted a connection.
///
/// The reply is read the way the client reads it: the length field says
/// how much follows, and the range sits in the fixed part.
fn read_resource_range(stream: &mut RawStream, request: &[u8]) -> Option<(u32, u32)> {
    use super::x11wire::{ByteOrder, SetupReplyWatcher};
    use std::io::Read;

    let mut watcher = SetupReplyWatcher::new();
    let order = ByteOrder::from_request(request.first().map(std::slice::from_ref)?)?;
    let mut buffer = [0u8; 4096];
    // The reply's length field counts at most 65535 units of four bytes;
    // the fixed part arrives first, so a few reads are enough.
    for _ in 0..8 {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        if let Some(range) = watcher.observe(&buffer[..read], order) {
            return Some(range);
        }
    }
    None
}

/// A plain socket to an X server, in either address form.
///
/// The range reading needs nothing more than a byte stream, so it uses
/// the ordinary sockets rather than the X11 library's own.
enum RawStream {
    /// The server over TCP.
    Tcp(std::net::TcpStream),
    /// The server over a unix socket.
    Unix(std::os::unix::net::UnixStream),
}

impl RawStream {
    /// Connect to the X server at this address.
    fn connect(address: &str) -> Option<Self> {
        if address.starts_with('/') {
            Some(Self::Unix(
                std::os::unix::net::UnixStream::connect(address).ok()?,
            ))
        } else {
            Some(Self::Tcp(std::net::TcpStream::connect(address).ok()?))
        }
    }
}

impl std::io::Read for RawStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl std::io::Write for RawStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buf),
            Self::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Unix(stream) => stream.flush(),
        }
    }
}

/// Connect to an X server.
///
/// Both address forms are accepted: a unix socket path (the form the X
/// server itself opens) and a `host:port` pair (the form the product
/// is configured with). A machine whose X server listens only on a unix
/// socket needs no bridge.
fn connect_x11_stream(address: &str) -> Option<x11rb::rust_connection::DefaultStream> {
    use x11rb::rust_connection::DefaultStream;
    use x11rb_protocol::parse_display::ConnectAddress;

    let (stream, _) = if address.starts_with('/') {
        DefaultStream::connect(&ConnectAddress::Socket(address.to_string())).ok()?
    } else {
        let (host, port) = address.rsplit_once(':')?;
        let port: u16 = port.parse().ok()?;
        DefaultStream::connect(&ConnectAddress::Hostname(host, port)).ok()?
    };
    Some(stream)
}

/// Open a connection to the X server, create a window on it, focus that
/// window, and return the range the connection owns.
///
/// The window is created with the X11 client library, which knows the
/// requests and the reply layouts.
///
/// The connection must stay OPEN: a window belongs to the connection
/// that created it, and the server destroys the window when that
/// connection closes. The focus would then fall back to the pointer
/// root, and the test would measure nothing.
///
/// So the connection is handed back to the caller, which keeps it alive
/// for as long as the focus is being measured.
fn create_focused_window(address: &str) -> Option<(u32, u32, u32, std::thread::JoinHandle<()>)> {
    // The X11 client library is a blocking one, so the window lives on
    // its own thread.
    let authorization = test_authorization();
    let address = address.to_string();
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        open_window_and_focus(&address, &authorization, &sender);
    });
    let (base, mask, window) = receiver.recv().ok()?;
    Some((base, mask, window, handle))
}

/// Create a window on a connection of its own and focus it, then report
/// the range and the window's id.
///
/// The connection stays open for as long as this function runs: the
/// window belongs to it, and the server destroys the window when the
/// connection closes.
fn open_window_and_focus(
    address: &str,
    authorization: &Authorization,
    sender: &std::sync::mpsc::Sender<(u32, u32, u32)>,
) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
    use x11rb::rust_connection::RustConnection;

    // The connection presents the test's own cookie: the library's
    // `connect` would read the user's `~/.Xauthority`, which the test
    // must not do behind their back.
    //
    // The library's own stream is used — it speaks both the TCP and the
    // unix socket forms of an X server address.
    let Some(stream) = connect_x11_stream(address) else {
        return;
    };
    let Ok(connection) = RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        authorization.name.clone(),
        authorization.data.clone(),
    ) else {
        return;
    };

    // The resource range this connection owns.
    let setup = connection.setup();
    let (base, mask) = (setup.resource_id_base, setup.resource_id_mask);
    let Some(screen) = setup.roots.first().cloned() else {
        return;
    };

    // A window on THIS connection: the id is allocated from this
    // connection's resource range.
    let Ok(window) = connection.generate_id() else {
        return;
    };
    if connection
        .create_window(
            screen.root_depth,
            window,
            screen.root,
            0,
            0,
            64,
            64,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new(),
        )
        .is_err()
    {
        return;
    }
    if connection.map_window(window).is_err() {
        return;
    }

    // The window must be VIEWABLE before it can take the focus: the
    // server refuses to focus a window that is not on the screen, and on
    // a composited server the window becomes viewable only once the
    // compositor has it. So the state is waited for.
    let mut viewable = false;
    for _ in 0..100 {
        let Ok(cookie) = connection.get_window_attributes(window) else {
            return;
        };
        let Ok(attributes) = cookie.reply() else {
            return;
        };
        if attributes.map_state == x11rb::protocol::xproto::MapState::VIEWABLE {
            viewable = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !viewable {
        return;
    }

    // The focus itself. The server answers with an error if the window
    // is still not viewable, and an error would be silent otherwise —
    // the focus would simply stay where it was.
    let Ok(cookie) =
        connection.set_input_focus(x11rb::protocol::xproto::InputFocus::PARENT, window, 0u32)
    else {
        return;
    };
    if cookie.check().is_err() {
        return;
    }

    if sender.send((base, mask, window)).is_err() {
        return;
    }

    // The connection stays open: the window belongs to it.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
#[tokio::test]
#[ignore = "needs an X server; run with RX11_TEST_X11 set"]
async fn the_focus_names_the_connection_that_created_the_focused_window() {
    let Some(address) = display_address() else {
        eprintln!("no X server to talk to; skipping");
        return;
    };

    // The setup request, and the range a connection of its own gets.
    let request = setup_request();
    let mut watcher = SetupRequestWatcher::new();
    watcher.observe(&request);

    // A connection of its own, to read the range the server grants.
    let mut stream = RawStream::connect(&address).expect("the X server accepts");
    {
        use std::io::Write;
        stream
            .write_all(&request)
            .expect("the setup request is sent");
        stream.flush().expect("the setup request is flushed");
    }
    let (base, mask) = read_resource_range(&mut stream, &request).expect("the setup reply");
    println!(
        "a connection's resource range: {base:#010x} .. {:#010x}",
        base | mask
    );

    // A window on a connection of its own, focused. The connection
    // stays open: the window belongs to it, and the server destroys the
    // window when the connection closes.
    let (window_base, window_mask, window, _keeper) = create_focused_window(&address)
        .expect("a window is created and focused on its own connection");
    println!("the focused window: {window:#010x}");
    println!(
        "the window's connection range: {window_base:#010x} .. {:#010x}",
        window_base | window_mask
    );

    // The focus, asked through the very monitor the serve loop uses.
    let mut monitor = ResidentMonitor::new(&address, test_authorization());
    let focus = monitor
        .focus_window()
        .await
        .expect("the monitor gets an answer");
    println!("the focus: {focus:#010x}");

    assert_eq!(
        focus, window,
        "the focus names the window that was created and focused"
    );

    // And the attribution: the focus window's id is looked up in the
    // ranges, and the connection that owns the range comes back.
    let mut index = ResourceIndex::default();
    index.register(
        ConnectionId::new(42),
        ResourceRange::new(window_base, window_mask),
    );
    let foreground = foreground_connection(focus, &index);
    println!("the foreground connection: {foreground:?}");
    assert_eq!(
        foreground,
        Some(ConnectionId::new(42)),
        "the focused window is traced back to the connection that created it"
    );
}
