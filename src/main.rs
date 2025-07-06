/* SAFETY:
 * We're using static mut refs in a safe manner, where only one element is modified by a "thread".
 * We could modify code to work on pointers, but assuming that the lib/exe is going to be used with
 * exclusive access, refs are the easiest of available options.
 */
#![allow(static_mut_refs)]

use core::ffi::{*};
use std::ffi::CString;
use std::io::{self, prelude::*};
use std::sync::Arc;
use std::str;
use std::sync::{TryLockResult, MutexGuard, Mutex};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::fs::File;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use tokio::task::JoinHandle;
use std::path::Path;
use tokio::{spawn, net::{TcpSocket, TcpStream}};

mod server;
use crate::server::start_server;

pub const REQUESTING_FILE: u8 = 0;
pub const REQUESTING_DIRS: u8 = 1;

const FROM_PORT_START: u16 = 49152;

pub const fn get_local_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127,0,0,1)), port)
}

async fn send_request_type(stream: &mut TcpStream, ttype: u8) -> Result<(), io::Error> {
    if let Err(e) = stream.write(&ttype.to_be_bytes()).await {
        eprintln!("ERROR: can't initialize request to server");
        return Err(e);
    }
    Ok(())
}

#[repr(C)]
pub struct TfFileNative {
    path: *const c_char,
    saving_name: *const c_char
}

struct TfFile<'a> {
    content: Vec<u8>,
    path: &'a str,
    saving_name: &'a str,
}

impl<'a> TfFile<'a> {
    fn new(path: &'a str, saving_name: &'a str) -> TfFile<'a> {
        TfFile {
            content: Vec::<u8>::new(),
            path,
            saving_name,
        }
    }

    fn from_native(f: &TfFileNative, vec: &'a mut Vec<String>) -> Result<Self, std::string::FromUtf8Error> {
        unsafe {
            let path = String::from_utf8(Vec::from(CStr::from_ptr(f.path).to_bytes()))?;
            let saving_name = String::from_utf8(Vec::from(CStr::from_ptr(f.saving_name).to_bytes()))?;
            vec.push(path);
            vec.push(saving_name);
        }

        Ok(Self::new(
            vec[vec.len() - 1].as_str(),
            vec[vec.len() - 2].as_str()
        ))
    }

    fn from_path(path: &'a Path) -> Result<TfFile<'a>, io::Error> {
        let path_checked = match path.to_str() {
            Some(path) => path,
            None => { return Err(io::Error::new(io::ErrorKind::InvalidData, "Fail to convert to UTF8")); },
        };
        let name = match path.file_name() {
            Some(name) => name,
            None => { return Err(io::Error::new(io::ErrorKind::InvalidData, "Couldn't extract file name")); },
        }
        .to_str().unwrap(); // whole path is valid utf8

        Ok(Self::new(path_checked, name))
    }

    fn save(&self) -> Result<(), io::Error> {
        assert!(!self.saving_name.is_empty());
        
        let mut osfile = File::create(self.saving_name)?;
        osfile.write_all(self.content.as_slice())?;

        Ok(())
    }

    async fn make_request(&mut self, from_port: u16, to_addr: SocketAddr) -> Result<(), io::Error> {
        let socket = match TcpSocket::new_v4() {
            Ok(sock) => sock,
            Err(error) => {
                eprintln!("ERROR: Couldn't create socket: {}", error);
                return Err(error);
            }
        };

        if let Err(error) = socket.bind(get_local_addr(from_port)) {
            eprintln!("ERROR: Couldn't bind socket: {}", error);
            return Err(error);
        }
        

        let mut stream = match socket.connect(to_addr).await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("ERROR: Couldn't connect to {to_addr}: {error}");
                return Err(error);
            }
        };

        send_request_type(&mut stream, REQUESTING_FILE).await?;

        if let Err(error) = stream.write_all(self.path.as_bytes()).await {
            eprintln!("ERROR: can't request file \"{}\": {}", self.path, error);
            return Err(error);
        }

        if let Err(error) = stream.shutdown().await {
            eprintln!("ERROR: Couldn't shutdown properly: {}", error);
            return Err(error);
        }

        match stream.read_to_end(&mut self.content).await {
            Err(error) => {
                eprintln!("ERROR: can't get file \"{}\": {}", self.path, error);
                return Err(error);
            },
            Ok(0) => {
                eprintln!("ERROR: got an empty response, file \"{}\" not saved", self.path);
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, ""));
            },
            Ok(_) => {},
        }

        if let Err(error) = self.save() {
            eprintln!("ERROR: file \"{}\" not saved: {}", self.path, error);
        } else {
            eprintln!("LOG: Saved \"{}\" in current directory", self.path);
        }
        
        Ok(())
    } 
}

fn spawn_requests_(file_in_slice: &'static mut [TfFile], rest: &'static mut [TfFile], from_port: u16, to_addr: SocketAddr, tasks: &'static mut Vec<JoinHandle<Result<(), io::Error>>>) {
    // unsafe: boundaries checked above 
    let file = unsafe { file_in_slice.get_unchecked_mut(0) };
    tasks.push(spawn(
        file.make_request(from_port, to_addr)
    ));

    if rest.is_empty() { return; }
    let (left, right) = rest.split_at_mut(1);
    spawn_requests_(left, right, from_port + 1, to_addr, tasks);
}

fn spawn_requests(files: &'static mut Vec<TfFile<'_>>, from_port_start: u16, to_addr: SocketAddr, tasks: &'static mut Vec<JoinHandle<Result<(), io::Error>>>) {
    if files.is_empty() { return; }
    let (left, right) = files.split_at_mut(1);
    spawn_requests_(left, right, from_port_start, to_addr, tasks);
}

/* Could use Arc<Mutex> or Arc<RwLock> or raw pointers */
/* Could rethink whole design to easily deal with it asyncly */

/* Meant only for OUTPUT_STRING variable to 'lazy-initialize it.
 *  TODO: Check if it's a bad practice and, if so, what cons it has
 */
enum LazyOutputString {
    Init(CString),
    Uninit(),
}

static mut PROCESSING: Mutex<()> = Mutex::new(());
static mut FILES: Vec<TfFile> = Vec::new();
static mut ARGS: Vec<String> = Vec::new();
static mut TASKS: Vec<JoinHandle<Result<(), io::Error>>> = Vec::new();
static mut TO_ADDR: SocketAddr = get_local_addr(0);
static mut OUTPUT_STRING: LazyOutputString = LazyOutputString::Uninit();
static mut OUTPUT_C_CHAR_PTR: *const c_char = 0 as *const c_char;

enum InputMode {
    ShowHelp,
    QueryList,
    RequestFiles,
    ServeCWD,
}

struct InputState {
    mode: InputMode,
    args: Vec<String>,
    addr: String,
}

impl InputState {
    fn from_mode(mode: InputMode) -> Self {
        InputState {
            mode,
            args: Vec::<String>::new(),
            addr: String::new(),
        }
    }

    fn from_mode_args(mode: InputMode, mut args: std::env::Args) -> Self {
        match args.next() {
            Some(addr) => InputState {
                mode,
                args: args.collect(),
                addr
            },
            None => Self::from_mode(InputMode::ShowHelp),
        }
    }

    fn from_args(mut args: std::env::Args) -> Self {
        if args.next().is_none() { /* TODO: check if no './tfrecv'*/
            return Self::from_mode(InputMode::ShowHelp);
        }

        match args.next() {
            None => { Self::from_mode(InputMode::ShowHelp) },
            Some(arg) if arg == "help" => {
                Self::from_mode(InputMode::ShowHelp)
            }
            Some(arg) if arg == "list" => {
                Self::from_mode_args(InputMode::QueryList, args)
            }
            Some(arg) if arg == "download" => {
                Self::from_mode_args(InputMode::RequestFiles, args)
            }
            Some(arg) if arg == "serve" => {
                Self::from_mode_args(InputMode::ServeCWD, args)
            },
            Some(_arg) => {
                Self::from_mode(InputMode::ShowHelp)
            }
        }
    }
}

fn show_help() {
    eprintln!(
"Query and download files from TCP server.
tfrecv (help|list|download|serve) IP_ADDRESS [dirs|paths to files]
    help: show this help message,
    list: query which files are available to download from specifies dirs,
    download: download specified files, saved in CWD
    serve: serve current working directory, passed dirs/files are ignored"
    );
}

async fn query_lists(args: &[String], addr: SocketAddr) -> Result<*const c_char, io::Error> {
    let sizes: Arc<[u8]> = args.iter().flat_map(|arg| {
        (arg.len() as u32).to_be_bytes()
    }).collect();
    let mut stream = match TcpStream::connect(addr).await {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("ERROR: Couldn't connect to \"{addr}\": {e}");
            return Err(e);
        }
    };

    if let Err(e) = send_request_type(&mut stream, REQUESTING_DIRS).await {
        eprintln!("ERROR: Couldn't send request type: {e}");
        return Err(e);
    }

    if let Err(e) = stream.write_all((args.len() as u32).to_be_bytes().as_slice()).await {
        eprintln!("ERROR: Couldn't send directory list to \"{addr}\": {e}");
        return Err(e);
    }

    if let Err(e) = stream.write_all(&sizes).await {
        eprintln!("ERROR: Couldn't send directory list to \"{addr}\": {e}");
        return Err(e);
    }

    for path in args.iter() {
        if let Err(e) = stream.write_all(path.as_str().as_bytes()).await {
            eprintln!("ERROR: Couldn't request directory \"{path}\": {e}");
        return Err(e);
        }
    }

    if let Err(e) = stream.shutdown().await {
        eprintln!("ERROR: Couldn't properly shutdown stream: {e}");
        return Err(e);
    }

    let mut resp = String::new();

    match stream.read_to_string(&mut resp).await {
        Err(e) => {
            eprintln!("ERROR: Couldn't read response from server: {e}");
            return Err(e);
        }
        Ok(0) => {
            eprintln!("ERROR: Received empty response from server");
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, ""));
        },
        Ok(_) => {},
    }

    // SAFETY: query_lists is called with mutual exclusion
    unsafe {
        let cstring = match CString::new(resp) {
            Err(e) => {
                eprintln!("ERROR: Couldn't convert response to c string: {e}");
                return Err(io::Error::new(io::ErrorKind::InvalidData, ""));
            },
            Ok(c) => c,
        };
        OUTPUT_STRING = LazyOutputString::Init(cstring);
        OUTPUT_C_CHAR_PTR = match &OUTPUT_STRING {
            LazyOutputString::Init(c) => c.as_ptr(),
            LazyOutputString::Uninit() => unreachable!(),
        };
        Ok(OUTPUT_C_CHAR_PTR)
    }
}

// SAFETY: mutex's used for thread-safety
unsafe fn lock_and_prepare_files_and_args() -> TryLockResult<MutexGuard<'static, ()>> {
    let res = PROCESSING.try_lock();
    if res.is_err() { return res; }

    FILES.clear();
    ARGS.clear();

    res
}

// SAFETY: External function to request multiple function
#[no_mangle]
pub unsafe extern "C" fn request_blocking(files: *const TfFileNative, count: c_ulong, to_addr: *const c_char, list_dirs: c_int) -> *const c_char {
    const NULL: *const c_char = std::ptr::null::<c_char>();

    let _mutex_guard = match lock_and_prepare_files_and_args() { 
        Err(_) => { return NULL; }
        Ok(g) => g,
    };

    let nfiles_sl = &*std::ptr::slice_from_raw_parts(files, count as usize);
    for nfile in nfiles_sl {
        match TfFile::from_native(nfile, &mut ARGS) {
            Ok(file) => FILES.push(file),
            Err(e) => {
                eprintln!("Couldn't get valid information from passed file: {e}");
                return NULL;
            }
        }
    }

    match CStr::from_ptr(to_addr).to_str() {
        Err(e) => {
            eprintln!("ERROR: Couldn't parse to_addr: {e}");
            return NULL;
        },
        Ok(a) => TO_ADDR = match a.parse::<SocketAddr>() {
            Err(e) => {
                eprintln!("ERROR: Couldn't parse to_addr: {e}");
                return NULL;
            },
            Ok(a) => a,
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    if list_dirs != 0 {
        runtime
            .block_on(query_lists(&ARGS, TO_ADDR))
            .unwrap_or(NULL)
    } else {
        runtime
            .block_on(async move {
                spawn_requests(&mut FILES, FROM_PORT_START, TO_ADDR, &mut TASKS);

                let mut success_all: c_int = 1;
                for task in TASKS.iter_mut() {
                    success_all &= task.await.is_ok() as c_int;
                }
                success_all as *const c_char
            })
    }
}

#[tokio::main]
async fn main() {
    unsafe { // only main thread at this point
        match InputState::from_args(std::env::args()) {
            InputState { mode: InputMode::ShowHelp, args: _ , addr: _ } => {
                show_help();
                return;
            },
            InputState { mode: InputMode::QueryList, args, addr } => {
                match addr.parse::<SocketAddr>() {
                    Ok(addr) => _ = query_lists(args.as_slice(), addr).await,
                    Err(e) => eprintln!("ERROR: Invalid addr: {}", e),
                }
                return;
            }
            InputState { mode: InputMode::RequestFiles, args, addr } => {
                match addr.parse::<SocketAddr>() {
                    Ok(addr) => { TO_ADDR = addr; },
                    Err(e) => {
                        eprintln!("ERROR: Invalid addr: {}", e);
                        return;
                    }
                }
                ARGS = args;
            }
            InputState { mode: InputMode::ServeCWD, args: _, addr } => {
                start_server(addr).await;
                return;
            }
        }

        FILES = ARGS.iter()
            .map(|arg| {
                TfFile::from_path(Path::new(arg.as_str()))
            })
            .zip(&ARGS)
            .filter_map(|res| {
                if res.0.is_err() {
                    eprintln!("ERROR: At \"{}\": {}", 
                        res.1, res.0.unwrap_err_unchecked());
                    return None;
                }

                Some(res.0.unwrap_unchecked()) 
            })
            .collect();

        spawn_requests(&mut FILES, FROM_PORT_START, TO_ADDR, &mut TASKS);

        for task in TASKS.iter_mut() {
            _ = task.await.unwrap();
        }
    }
}
