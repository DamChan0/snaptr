mod app;
mod font;
mod io;
mod logic;
mod paint;

fn print_usage() {
    eprintln!("usage: snaptr [--full] [--to <lang>] [--ocr <tesseract langs>]");
}

fn main() {
    let mut full = false;
    let mut to = "ko".to_string();
    let mut ocr = "eng".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--full" => full = true,
            "--to" => {
                to = match args.next() {
                    Some(v) => v,
                    None => {
                        eprintln!("--to requires an argument");
                        print_usage();
                        std::process::exit(2);
                    }
                };
            }
            "--ocr" => {
                ocr = match args.next() {
                    Some(v) => v,
                    None => {
                        eprintln!("--ocr requires an argument");
                        print_usage();
                        std::process::exit(2);
                    }
                };
            }
            "-h" | "--help" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    if let Err(e) = app::run(full, &to, &ocr) {
        eprintln!("snaptr: {e}");
        std::process::exit(1);
    }
}
