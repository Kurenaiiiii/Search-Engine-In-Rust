use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut addr = std::env::var("SE_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let mut dir = std::env::var("SE_DATA").unwrap_or_else(|_| "./data".to_string());
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--addr" => {
                i += 1;
                addr = args.get(i).cloned().unwrap_or(addr);
            }
            "--dir" | "--data" => {
                i += 1;
                dir = args.get(i).cloned().unwrap_or(dir);
            }
            "--help" | "-h" => {
                println!("se-server [--addr 127.0.0.1:3000] [--dir ./data]");
                println!("env: SE_ADDR, SE_DATA, SE_TOKEN (bearer auth, optional)");
                return;
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if let Err(e) = se_server::run(&addr, &PathBuf::from(&dir)) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
