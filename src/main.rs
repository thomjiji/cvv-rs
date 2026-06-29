use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process;

const BUFFER_SIZE: usize = 8 * 1024 * 1024; // 8 MB

fn copy_file(source: &Path, destination: &Path) -> Result<u64, std::io::Error> {
    let mut src = File::open(source)?;
    let mut dst = File::create(destination)?;
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut total_bytes: u64 = 0;

    loop {
        let bytes_read = src.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        dst.write_all(&buffer[..bytes_read])?;
        total_bytes += bytes_read as u64;
    }

    println!("about to return");
    Ok(total_bytes)
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 {
        eprintln!("Usage: cvv <source> <destination>");
        process::exit(1);
    }

    let source = Path::new(&args[1]);
    let destination = Path::new(&args[2]);

    let destination = if destination.is_dir() {
        let file_name = source.file_name().unwrap_or_else(|| {
            eprintln!("Error: cannot extract file name from source");
            process::exit(1);
        });
        destination.join(file_name)
    } else {
        destination.to_path_buf()
    };

    match copy_file(source, &destination) {
        Ok(bytes) => println!(
            "Copied {} -> {}  ({} bytes)",
            source.display(),
            destination.display(),
            bytes
        ),
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    }
}
