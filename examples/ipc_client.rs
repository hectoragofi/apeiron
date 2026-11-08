//! A tiny client for Apeiron's IPC pipe (see `docs/IPC.md`).
//!
//!     cargo run --example ipc_client                      # hello + get_layout
//!     cargo run --example ipc_client -- '{"op":"focus_window","hwnd":1234}'
//!     cargo run --example ipc_client -- --watch           # subscribe, print events
//!
//! Each argument is sent as one request (an `id` is added if missing) and its
//! reply printed. `--pipe NAME` talks to another pipe than `\\.\pipe\apeiron-v1`.

use std::io::{BufRead, BufReader, Write};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut pipe = r"\\.\pipe\apeiron-v1".to_string();
    if let Some(i) = args.iter().position(|a| a == "--pipe") {
        args.remove(i);
        if i < args.len() {
            pipe = args.remove(i);
        }
    }
    let watch = args.iter().any(|a| a == "--watch");
    args.retain(|a| a != "--watch");
    if args.is_empty() && !watch {
        args = vec![r#"{"op":"hello"}"#.into(), r#"{"op":"get_layout"}"#.into()];
    }
    if watch {
        args.push(r#"{"op":"subscribe"}"#.into());
    }

    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pipe)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("can't open {pipe}: {e} (is Apeiron running?)");
            std::process::exit(1);
        }
    };
    let mut reader = BufReader::new(file.try_clone().expect("clone pipe handle"));
    for (n, arg) in args.iter().enumerate() {
        let mut req: serde_json::Value = match serde_json::from_str(arg) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("not JSON: {arg}: {e}");
                std::process::exit(2);
            }
        };
        if req.get("id").is_none() {
            req["id"] = (n + 1).into();
        }
        println!("> {req}");
        writeln!(file, "{req}").expect("write request");
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            eprintln!("pipe closed");
            return;
        }
        println!("< {}", line.trim_end());
    }
    if watch {
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            println!("{}", line.trim_end());
            line.clear();
        }
    }
}
