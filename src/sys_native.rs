use std::{
    any::Any,
    env,
    fs::{self, File},
    io::{stderr, stdin, stdout, Read, Write},
    net::*,
    path::Path,
    process::Command,
    slice,
    sync::atomic::{self, AtomicU64},
    thread::sleep,
    time::Duration,
};

use crate::{Handle, SysBackend};
use bufreaderwriter::seq::BufReaderWriterSeq;
use dashmap::DashMap;
use once_cell::sync::Lazy;

/// The defualt native system backend
#[derive(Default)]
pub struct NativeSys;

type Buffered<T> = BufReaderWriterSeq<T>;

struct GlobalNativeSys {
    next_handle: AtomicU64,
    files: DashMap<Handle, Buffered<File>>,
    tcp_listeners: DashMap<Handle, TcpListener>,
    tcp_sockets: DashMap<Handle, Buffered<TcpStream>>,
    hostnames: DashMap<Handle, String>,
    #[cfg(feature = "audio")]
    audio_stream_time: parking_lot::Mutex<Option<f64>>,
    #[cfg(feature = "audio")]
    audio_time_socket: parking_lot::Mutex<Option<std::sync::Arc<std::net::UdpSocket>>>,
    colored_errors: DashMap<String, String>,
}

enum SysStream<'a> {
    File(dashmap::mapref::one::RefMut<'a, Handle, Buffered<File>>),
    TcpListener(dashmap::mapref::one::RefMut<'a, Handle, TcpListener>),
    TcpSocket(dashmap::mapref::one::RefMut<'a, Handle, Buffered<TcpStream>>),
}

impl Default for GlobalNativeSys {
    fn default() -> Self {
        Self {
            next_handle: Handle::FIRST_UNRESERVED.0.into(),
            files: DashMap::new(),
            tcp_listeners: DashMap::new(),
            tcp_sockets: DashMap::new(),
            hostnames: DashMap::new(),
            #[cfg(feature = "audio")]
            audio_stream_time: parking_lot::Mutex::new(None),
            #[cfg(feature = "audio")]
            audio_time_socket: parking_lot::Mutex::new(None),
            colored_errors: DashMap::new(),
        }
    }
}

impl GlobalNativeSys {
    fn new_handle(&self) -> Handle {
        for _ in 0..u64::MAX {
            let handle = Handle(self.next_handle.fetch_add(1, atomic::Ordering::Relaxed));
            if !self.files.contains_key(&handle)
                && !self.tcp_listeners.contains_key(&handle)
                && !self.tcp_sockets.contains_key(&handle)
            {
                return handle;
            }
        }
        panic!("Ran out of file handles");
    }
    fn get_stream(&self, handle: Handle) -> Result<SysStream, String> {
        Ok(if let Some(file) = self.files.get_mut(&handle) {
            SysStream::File(file)
        } else if let Some(listener) = self.tcp_listeners.get_mut(&handle) {
            SysStream::TcpListener(listener)
        } else if let Some(socket) = self.tcp_sockets.get_mut(&handle) {
            SysStream::TcpSocket(socket)
        } else {
            return Err("Invalid file handle".to_string());
        })
    }
}

static NATIVE_SYS: Lazy<GlobalNativeSys> = Lazy::new(Default::default);

#[cfg(feature = "audio")]
#[doc(hidden)]
pub fn set_audio_stream_time(time: f64) {
    *NATIVE_SYS.audio_stream_time.lock() = Some(time);
}

#[cfg(feature = "audio")]
#[doc(hidden)]
pub fn set_audio_stream_time_port(port: u16) -> std::io::Result<()> {
    let socket = std::net::UdpSocket::bind(("127.0.0.1", 0))?;
    socket.connect(("127.0.0.1", port))?;
    *NATIVE_SYS.audio_time_socket.lock() = Some(std::sync::Arc::new(socket));
    Ok(())
}

impl SysBackend for NativeSys {
    fn any(&self) -> &dyn Any {
        self
    }
    fn any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn print_str_stdout(&self, s: &str) -> Result<(), String> {
        let mut stdout = stdout().lock();
        stdout.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())
    }
    fn print_str_stderr(&self, s: &str) -> Result<(), String> {
        let mut stderr = stderr().lock();
        stderr.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
        stderr.flush().map_err(|e| e.to_string())
    }
    fn scan_line_stdin(&self) -> Result<Option<String>, String> {
        let mut buffer = Vec::new();
        let mut b = 0u8;
        loop {
            if let Err(e) = stdin().read_exact(slice::from_mut(&mut b)) {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Ok(None);
                }
                return Err(e.to_string());
            }

            match b {
                #[cfg(feature = "raw_mode")]
                b'\r'
                    if crossterm::terminal::is_raw_mode_enabled().map_err(|e| e.to_string())? =>
                {
                    break
                }
                b'\r' => continue,
                b'\n' | 3 => break,
                b => buffer.push(b),
            }
        }
        Ok(Some(String::from_utf8(buffer).map_err(|e| e.to_string())?))
    }
    fn save_error_color(&self, message: String, colored: String) {
        NATIVE_SYS.colored_errors.insert(message, colored);
    }
    fn term_size(&self) -> Result<(usize, usize), String> {
        let (w, h) = terminal_size::terminal_size()
            .ok_or("Failed to get terminal size")
            .map(|(w, h)| (w.0 as usize, h.0 as usize))?;
        Ok((w, h.saturating_sub(1)))
    }
    #[cfg(feature = "raw_mode")]
    fn set_raw_mode(&self, raw_mode: bool) -> Result<(), String> {
        if raw_mode {
            crossterm::terminal::enable_raw_mode()
        } else {
            crossterm::terminal::disable_raw_mode()
        }
        .map_err(|e| e.to_string())
    }
    fn var(&self, name: &str) -> Option<String> {
        env::var(name).ok()
    }
    fn file_exists(&self, path: &str) -> bool {
        fs::metadata(path).is_ok()
    }
    fn is_file(&self, path: &str) -> Result<bool, String> {
        fs::metadata(path)
            .map(|m| m.is_file())
            .map_err(|e| e.to_string())
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            paths.push(entry.path().to_string_lossy().into());
        }
        Ok(paths)
    }
    fn open_file(&self, path: &Path) -> Result<Handle, String> {
        let handle = NATIVE_SYS.new_handle();
        let file = File::open(path).map_err(|e| format!("{e} {}", path.display()))?;
        NATIVE_SYS.files.insert(handle, Buffered::new_reader(file));
        Ok(handle)
    }
    fn file_read_all(&self, path: &Path) -> Result<Vec<u8>, String> {
        let handle = self.open_file(path)?;
        let bytes = self.read(handle, usize::MAX)?;
        self.close(handle)?;
        Ok(bytes)
    }
    fn create_file(&self, path: &Path) -> Result<Handle, String> {
        let handle = NATIVE_SYS.new_handle();
        let file = File::create(path).map_err(|e| e.to_string())?;
        NATIVE_SYS.files.insert(handle, Buffered::new_writer(file));
        Ok(handle)
    }
    fn delete(&self, path: &str) -> Result<(), String> {
        let path = Path::new(path);
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(|e| e.to_string())
        } else {
            fs::remove_file(path).map_err(|e| e.to_string())
        }
    }
    #[cfg(feature = "trash")]
    fn trash(&self, path: &str) -> Result<(), String> {
        trash::delete(path).map_err(|e| e.to_string())
    }
    fn read(&self, handle: Handle, len: usize) -> Result<Vec<u8>, String> {
        Ok(match NATIVE_SYS.get_stream(handle)? {
            SysStream::File(mut file) => {
                let mut buf = Vec::new();
                Write::by_ref(&mut *file)
                    .take(len as u64)
                    .read_to_end(&mut buf)
                    .map_err(|e| e.to_string())?;
                buf
            }
            SysStream::TcpListener(_) => return Err("Cannot read from a tcp listener".to_string()),
            SysStream::TcpSocket(mut socket) => {
                let mut buf = Vec::new();
                Write::by_ref(&mut *socket)
                    .take(len as u64)
                    .read_to_end(&mut buf)
                    .map_err(|e| e.to_string())?;
                buf
            }
        })
    }
    fn write(&self, handle: Handle, conts: &[u8]) -> Result<(), String> {
        let mut conts = conts;
        let colored;
        if let Some(colored_error) = NATIVE_SYS
            .colored_errors
            .get(String::from_utf8_lossy(conts).as_ref())
            .as_deref()
            .cloned()
        {
            colored = colored_error;
            conts = colored.as_bytes();
        }
        match NATIVE_SYS.get_stream(handle)? {
            SysStream::File(mut file) => file.write_all(conts).map_err(|e| e.to_string()),
            SysStream::TcpListener(_) => Err("Cannot write to a tcp listener".to_string()),
            SysStream::TcpSocket(mut socket) => socket.write_all(conts).map_err(|e| e.to_string()),
        }
    }
    fn sleep(&self, seconds: f64) -> Result<(), String> {
        sleep(Duration::from_secs_f64(seconds));
        Ok(())
    }
    #[cfg(feature = "terminal_image")]
    fn show_image(&self, image: image::DynamicImage) -> Result<(), String> {
        let dim = terminal_size::terminal_size().map(|(w, h)| (w.0 as usize, h.0 as usize));
        let (width, height) = if let Some((w, h)) = dim {
            let (tw, th) = (w as u32, h.saturating_sub(1) as u32);
            let (iw, ih) = (image.width(), image.height() / 2);
            let scaled_to_height = (iw * th / ih.max(1), th);
            let scaled_to_width = (tw, ih * tw / iw.max(1));
            let (w, h) = if scaled_to_height.0 <= tw {
                scaled_to_height
            } else {
                scaled_to_width
            };
            (Some(w), Some(h))
        } else {
            (None, None)
        };
        viuer::print(
            &image,
            &viuer::Config {
                width,
                height,
                absolute_offset: false,
                transparent: true,
                ..Default::default()
            },
        )
        .map(drop)
        .map_err(|e| format!("Failed to show image: {e}"))
    }
    #[cfg(feature = "audio")]
    fn play_audio(&self, wav_bytes: Vec<u8>) -> Result<(), String> {
        use hodaun::*;
        match default_output::<Stereo>() {
            Ok(mut mixer) => {
                match wav::WavSource::new(std::collections::VecDeque::from(wav_bytes)) {
                    Ok(source) => {
                        mixer.add(source.resample());
                        mixer.block();
                        Ok(())
                    }
                    Err(e) => Err(format!("Failed to read wav bytes: {e}")),
                }
            }
            Err(e) => Err(format!("Failed to initialize audio output stream: {e}").to_string()),
        }
    }
    #[cfg(feature = "audio")]
    fn audio_sample_rate(&self) -> u32 {
        hodaun::default_output_device()
            .and_then(|device| {
                hodaun::cpal::traits::DeviceTrait::default_output_config(&device).ok()
            })
            .map(|config| config.sample_rate().0)
            .unwrap_or(44100)
    }
    #[cfg(feature = "audio")]
    fn stream_audio(&self, f: crate::AudioStreamFn) -> Result<(), String> {
        use hodaun::*;
        struct TheSource {
            time: f64,
            samples: std::vec::IntoIter<[f64; 2]>,
            f: crate::AudioStreamFn,
        }
        impl Source for TheSource {
            type Frame = Stereo;
            fn next(&mut self, sample_rate: f64) -> Option<Self::Frame> {
                if let Some([left, right]) = self.samples.next() {
                    return Some(Stereo { left, right });
                }
                const LEN: usize = 10000;
                let mut times = Vec::with_capacity(LEN);
                for _ in 0..LEN {
                    times.push(self.time);
                    self.time += 1.0 / sample_rate;
                }
                if let Some(socket) = NATIVE_SYS.audio_time_socket.lock().as_ref() {
                    if let Err(e) = socket.send(&self.time.to_be_bytes()) {
                        eprintln!("Failed to send audio time: {e}");
                    }
                }
                match (self.f)(&times) {
                    Ok(samples) => {
                        self.samples = samples.into_iter();
                        self.next(sample_rate)
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        None
                    }
                }
            }
        }
        let source = TheSource {
            time: NATIVE_SYS.audio_stream_time.lock().unwrap_or(0.0),
            samples: Vec::new().into_iter(),
            f,
        };
        match default_output::<Stereo>() {
            Ok(mut mixer) => {
                mixer.add(source);
                mixer.block();
                Ok(())
            }
            Err(e) => Err(format!("Failed to initialize audio output stream: {e}").to_string()),
        }
    }
    fn tcp_listen(&self, addr: &str) -> Result<Handle, String> {
        let handle = NATIVE_SYS.new_handle();
        let listener = TcpListener::bind(addr).map_err(|e| e.to_string())?;
        NATIVE_SYS.tcp_listeners.insert(handle, listener);
        Ok(handle)
    }
    fn tcp_accept(&self, handle: Handle) -> Result<Handle, String> {
        let listener = NATIVE_SYS
            .tcp_listeners
            .get_mut(&handle)
            .ok_or_else(|| "Invalid tcp listener handle".to_string())?;
        let (stream, _) = listener.accept().map_err(|e| e.to_string())?;
        drop(listener);
        let handle = NATIVE_SYS.new_handle();
        NATIVE_SYS
            .tcp_sockets
            .insert(handle, Buffered::new_reader(stream));
        Ok(handle)
    }
    fn tcp_connect(&self, addr: &str) -> Result<Handle, String> {
        let handle = NATIVE_SYS.new_handle();
        let stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        NATIVE_SYS
            .tcp_sockets
            .insert(handle, Buffered::new_writer(stream));
        NATIVE_SYS.hostnames.insert(
            handle,
            addr.split_once(':')
                .ok_or("No colon in address")?
                .0
                .to_string(),
        );
        Ok(handle)
    }
    fn tcp_addr(&self, handle: Handle) -> Result<String, String> {
        let socket = NATIVE_SYS
            .tcp_sockets
            .get(&handle)
            .ok_or_else(|| "Invalid tcp socket handle".to_string())?;
        Ok(socket
            .get_ref()
            .peer_addr()
            .map_err(|e| e.to_string())?
            .to_string())
    }
    fn tcp_set_non_blocking(&self, handle: Handle, non_blocking: bool) -> Result<(), String> {
        let socket = NATIVE_SYS
            .tcp_sockets
            .get(&handle)
            .ok_or_else(|| "Invalid tcp socket handle".to_string())?;
        socket
            .get_ref()
            .set_nonblocking(non_blocking)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn tcp_set_read_timeout(
        &self,
        handle: Handle,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        let socket = NATIVE_SYS
            .tcp_sockets
            .get(&handle)
            .ok_or_else(|| "Invalid tcp socket handle".to_string())?;
        socket
            .get_ref()
            .set_read_timeout(timeout)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn tcp_set_write_timeout(
        &self,
        handle: Handle,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        let socket = NATIVE_SYS
            .tcp_sockets
            .get(&handle)
            .ok_or_else(|| "Invalid tcp socket handle".to_string())?;
        socket
            .get_ref()
            .set_write_timeout(timeout)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn close(&self, handle: Handle) -> Result<(), String> {
        if NATIVE_SYS.files.remove(&handle).is_some()
            || NATIVE_SYS.tcp_listeners.remove(&handle).is_some()
            || NATIVE_SYS.tcp_sockets.remove(&handle).is_some()
        {
            NATIVE_SYS.hostnames.remove(&handle);
            Ok(())
        } else {
            Err("Invalid stream handle".to_string())
        }
    }
    #[cfg(feature = "invoke")]
    fn invoke(&self, path: &str) -> Result<(), String> {
        open::that(path).map_err(|e| e.to_string())
    }
    fn run_command_inherit(&self, command: &str, args: &[&str]) -> Result<i32, String> {
        let status = Command::new(command)
            .args(args)
            .spawn()
            .map_err(|e| e.to_string())?
            .wait()
            .map_err(|e| e.to_string())?;
        Ok(status.code().unwrap_or(0))
    }
    fn run_command_capture(
        &self,
        command: &str,
        args: &[&str],
    ) -> Result<(i32, String, String), String> {
        let output = Command::new(command)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        Ok((
            output.status.code().unwrap_or(0),
            String::from_utf8_lossy(&output.stdout).into(),
            String::from_utf8_lossy(&output.stderr).into(),
        ))
    }
    fn change_directory(&self, path: &str) -> Result<(), String> {
        env::set_current_dir(path).map_err(|e| e.to_string())
    }
    #[cfg(feature = "https")]
    fn https_get(&self, request: &str, handle: Handle) -> Result<String, String> {
        let host = NATIVE_SYS
            .hostnames
            .get(&handle)
            .ok_or_else(|| "Invalid tcp socket handle".to_string())?
            .to_string();
        let request = check_http(request.to_string(), &host)?;

        // https://github.com/rustls/rustls/blob/c9cfe3499681361372351a57a00ccd793837ae9c/examples/src/bin/simpleclient.rs
        static CLIENT_CONFIG: Lazy<std::sync::Arc<rustls::ClientConfig>> = Lazy::new(|| {
            let mut store = rustls::RootCertStore::empty();
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder()
                .with_root_certificates(store)
                .with_no_client_auth()
                .into()
        });

        let mut socket = NATIVE_SYS
            .tcp_sockets
            .get_mut(&handle)
            .ok_or_else(|| "Invalid tcp socket handle".to_string())?;

        let server_name =
            rustls::pki_types::ServerName::try_from(host).map_err(|e| e.to_string())?;
        let tcp_stream = socket.get_mut();

        let mut conn = rustls::ClientConnection::new(CLIENT_CONFIG.clone(), server_name)
            .map_err(|e| e.to_string())?;
        let mut tls = rustls::Stream::new(&mut conn, tcp_stream);
        tls.write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut buffer = Vec::new();
        tls.read_to_end(&mut buffer).map_err(|e| e.to_string())?;
        let s = String::from_utf8(buffer).map_err(|e| {
            "Error converting HTTP Response to utf-8: ".to_string() + &e.to_string()
        })?;

        Ok(s)
    }
}

/// Takes an HTTP request, validates it, and fixes it (if possible) by adding
/// the HTTP version and trailing newlines if they aren't present.
///
/// Also adds a host header if one isn't present.
///
/// ```no_run
/// # fn check_http(a: String, b: &str) -> Result<String, String> { Ok(a) }
/// assert_eq!(
///     check_http("GET /".to_string(), "example.com").unwrap(),
///     "GET / HTTP/1.0\r\nhost: example.com\r\n\r\n"
/// )
/// ```
#[cfg(feature = "https")]
fn check_http(mut request: String, hostname: &str) -> Result<String, String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    let mut lines = request.lines().collect::<Vec<_>>();
    let mut trailing_newline = request.ends_with('\n');

    // check to make sure theres an empty line somewhere in there. if not, add 2 empty lines
    if !lines.iter().any(|line| line.is_empty()) {
        lines.push("");
        lines.push("");
        // so we dont unecessarily add another newline
        trailing_newline = false;
    }

    // If the first line doesn't have a version, add one
    let first = lines.first().ok_or("Empty HTTP request")?;
    let last_token = first
        .trim_end()
        .split_ascii_whitespace()
        .next_back()
        .ok_or("Empty first line")?;
    if !last_token.starts_with("HTTP/") {
        request = first.to_string()
            + " HTTP/1.0\r\n"
            + &lines.into_iter().skip(1).collect::<Vec<_>>().join("\r\n");
    } else {
        request = lines.join("\r\n");
    }
    if trailing_newline {
        request += "\r\n";
    }

    // Confirm that the request is valid
    let status = req.parse(request.as_bytes()).map_err(|e| {
        use httparse::Error;
        format!(
            "Failed to parse HTTP request: {}",
            match e {
                Error::HeaderName => "Invalid byte in header name",
                Error::HeaderValue => "Invalid byte in Header value",
                Error::NewLine => "Invalid byte in newline",
                Error::Status => "Invalid byte in response status",
                Error::Token => "Invalid byte where token is required",
                Error::TooManyHeaders => "Too many headers! Maximum of 64",
                Error::Version => "Invalid byte in HTTP version",
            }
        )
    })?;
    match status {
        httparse::Status::Partial => return Err("Incomplete (Partial) HTTP request".into()),
        httparse::Status::Complete(_) => {}
    };

    // If Status was Complete everything should be there
    // (but just in case we'll check)
    let _method = req.method.ok_or("No method in HTTP request")?;
    let _path = req.path.ok_or("No path in HTTP request")?;
    let _version = req.version.ok_or("No version in HTTP request")?;

    // add the host header
    // it's safe the unwrap here because if the http request is valid, it must
    // have a newline in it
    if !req
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("host"))
    {
        let newline = request.find('\n').unwrap();
        request.insert_str(newline + 1, &format!("host: {hostname}\r\n"));
    }

    Ok(request)
}
