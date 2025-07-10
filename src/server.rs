use std::io::{self, Read};
use std::path::{self, Path};
use std::net::SocketAddr;
use std::fs::File;
use tokio::{spawn, net::{TcpListener, TcpSocket, TcpStream}};
use tokio::io::{AsyncWriteExt, AsyncReadExt};

fn get_path_inside_cwd(path_str: &str) -> Result<path::PathBuf, io::Error> {
    let path = match Path::new(path_str).canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return Err(e);
        }
    };
    
    // We canonicalize it so that prefix "\\?\" on Windows is in both cwd and path
    let cwd = std::env::current_dir()
        .expect("Must return cwd")
        .canonicalize()
        .expect("CWD canonicalizes fine");
    if !path.starts_with(&cwd) {
        let err_str = format!("ERROR: Requested file \"{}\" is outside cwd = \"{}\"", path.display(), cwd.display());
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, err_str));
    }
    Ok(path)
}

// relies that path is the last thing sent
async fn extract_path_inside_cwd(stream: &mut TcpStream) -> Result<path::PathBuf, io::Error> {
    let path = {
        let mut path_str: String = String::new();
        if let Err(e) = stream.read_to_string(&mut path_str).await {
            eprintln!("ERROR: Couldn't get path to requested file: {e}");
            return Err(e);
        }
        path_str
    };
    get_path_inside_cwd(path.as_str())
}

async fn handle_file(mut stream: TcpStream, from_addr: SocketAddr) {
    let path = match extract_path_inside_cwd(&mut stream).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: connection {from_addr}: couldn't get path to file from client: {e}");
            return;
        }
    };

    eprintln!("LOG: got {} from {from_addr}", path.display());

    let mut buf = vec![];
    let mut file = match File::open(&path) {
        Err(e) => {
            eprintln!("ERROR: Couldn't open file \"{}\": {e}", path.display());
            return;
        }
        Ok(f) => f,
    };

    match file.read_to_end(&mut buf) {
        Err(e) => {
            eprintln!("ERROR: Couldn't read file \"{}\": {e}", path.display());
            return;
        },
        Ok(n) => {
            eprintln!("LOG: read {} for {from_addr} {n} bytes", path.display());
        }
    }

    if let Err(e) = stream.write_all(&buf).await {
        eprintln!("ERROR: Couldn't respond with file \"{}\": {e}", path.display());
        return;
    }
    eprintln!("LOG: connection {from_addr} handled successfully");
}

async fn handle_dirs(mut stream: TcpStream, from_addr: SocketAddr) {
    let dir_count = match stream.read_u32().await {
        Ok(c) => c as usize,
        Err(e) => {
            eprintln!("ERROR: Couldn't get strings count: {e}");
            return;
        }
    };

    eprintln!("LOG: dir_count = {dir_count} from {from_addr}");

    const U32_SIZE: usize = std::mem::size_of::<u32>();
    let mut dir_sizes: Vec<u8> = vec![0_u8; dir_count*U32_SIZE];
    let dir_sizes_sl = unsafe {
        dir_sizes.get_unchecked_mut(..dir_count*U32_SIZE)
    };


    match stream.read_exact(dir_sizes_sl).await {
        Ok(_) => {},
        Err(e) => {
            eprintln!("ERROR: Couldn't read dir sizes: {e}");
            return;
        }
    }

    let mut dir_paths: String = String::new();

    if let Err(e) = stream.read_to_string(&mut dir_paths).await {
        eprintln!("ERROR: Couldn't read dir paths: {e}");
        return;
    }

    let mut dir_paths_read: usize = 0;
    for byte_ind in (0..dir_sizes.len()).step_by(U32_SIZE) {
        let dir_size = u32::from_be_bytes(dir_sizes[byte_ind..byte_ind+U32_SIZE].try_into().unwrap()) as usize;
        let dir_path = &dir_paths[dir_paths_read..dir_paths_read+dir_size];
        dir_paths_read += dir_size;

        eprintln!("LOG: connection {from_addr}: dir_path = {dir_path}");

        let path_checked = match get_path_inside_cwd(dir_path) {
            Err(e) => {
                eprintln!("ERROR: fail to check \"{}\" with size {dir_size}: {e}", dir_path);
                continue;
            },
            Ok(p) => p,
        };

        if !path_checked.is_dir() {
            eprintln!("ERROR: \"{}\" is not a dir", path_checked.display());
            continue;
        }

        let entries = match path_checked.read_dir() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("ERROR: Couldn't get entries of \"{}\": {e}", path_checked.display());
                continue;
            }
        };

        for entry_res in entries {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue
            };

            let mut entry_buf = entry.path();

            match entry.file_type() {
                Err(e) => {
                    eprintln!("ERROR: Couldn't get type of \"{}\": {e}", entry.path().display());
                    continue;
                },
                Ok(ttype) if ttype.is_dir() => {
                    // Pushing "", empty string, appends '/' at the end
                    entry_buf.push("");
                }
                Ok(_) => {},
            }

            if let Err(e) = stream.write_all(entry_buf.to_str().expect("Entry path is a valid str").as_bytes()).await {
                eprintln!("ERROR: Couldn't write \"{}\" to stream: {e}", entry.path().display());
            }

            if let Err(e) = stream.write_all("\r\n".as_bytes()).await {
                eprintln!("ERROR: Couldn't write \"{}\" to stream: {e}", entry.path().display());
                // breaking because if fail to pass delimiters then further entries will concatenate with current one
                break;
            }
        }
    }

    if let Err(e) = stream.shutdown().await {
        eprintln!("ERROR: Couldn't properly shutdown stream: {e}");
    }
}

pub async fn start_server(addr: String) {
    let listener = match TcpListener::bind(addr.as_str()).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ERROR: can't bind to \"{addr}\": {e}");
            return;
        }
    };

    loop {
        let (mut stream, from_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ERROR: Couldn't get client: {e}");
                break;
            }
        };

        eprintln!("LOG: got connection from: {from_addr}");

        let ttype = match stream.read_u8().await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("ERROR: Couldn't get type: {e}");
                continue;
            },
        };
        eprintln!("LOG: connection {from_addr}: got request type = {ttype}");

        match ttype {
            crate::REQUESTING_FILE => _ = spawn(handle_file(stream, from_addr)),
            crate::REQUESTING_DIRS => _ = spawn(handle_dirs(stream, from_addr)),
            _ => {
                eprintln!("Got invalid request type = {ttype} from {from_addr}");
            }
        };
    }
    /*
        accept any connection
        if file request, serve on a separate coro
        if dir request, serve on a sep coro too
     */
}
