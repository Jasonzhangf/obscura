//! Deterministic HTTP page for manual Host and device replay.
//!
//! The fixture binds an ephemeral port by default, prints the URL, and keeps
//! serving until interrupted. The page is intentionally long enough to test
//! the mobile viewport and contains a form for input checks. Every response
//! sets the same cookie so a restarted Host can be checked by inspecting the
//! request headers or `document.cookie`.

use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Deterministic HTTP fixture for Obscura Host replay")]
struct Args {
    /// Address reachable by the Host process. Keep the default loopback for local-only replay.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// TCP port, or 0 to select an available port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

fn response(stream: &mut TcpStream) -> io::Result<()> {
    let mut request = [0_u8; 8192];
    let size = stream.read(&mut request)?;
    let request = String::from_utf8_lossy(&request[..size]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let body = if path == "/health" {
        "ok\n".to_string()
    } else {
        let mut body = String::from(
            "<!doctype html><meta name='viewport' content='width=device-width'>\
             <title>Obscura profile fixture</title><style>body{font:18px sans-serif;margin:0;background:white}#target{position:absolute;left:0;top:0;width:180px;height:180px;background:red;color:white;font-size:18px}#field{position:absolute;left:0;top:190px;width:220px;height:100px;font-size:18px;box-sizing:border-box}#meta{position:absolute;left:0;top:300px}#long{margin-top:400px;min-height:3000px;background:blue;color:white;padding:8px}</style>\
             <button id='target' onclick=\"window.clicked=(window.clicked||0)+1;this.style.background='lime'\">touch</button>\
             <input id='field' value='' placeholder='Persistent form'>\
             <section id='meta'><h1>Obscura profile fixture</h1><p>Cookie: <code id='cookie'>(pending)</code></p><p>Local storage: <code id='storage'>(pending)</code></p></section>\
             <div id='long'>",
        );
        for index in 0..40 {
            body.push_str(&format!("<p>Long page row {index}: stable content for mobile viewport and scroll replay.</p>"));
        }
        body.push_str(
            "</div><script>document.getElementById('cookie').textContent=document.cookie||'(empty)';\
             document.getElementById('storage').textContent=localStorage.getItem('m1')||'(empty)';\
             localStorage.setItem('m1','page-only');</script>",
        );
        body
    };

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nSet-Cookie: sid=from-server; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    )
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind((args.bind, args.port))?;
    println!("fixture_url=http://{}/", listener.local_addr()?);
    io::stdout().flush()?;
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = response(&mut stream) {
                    eprintln!("fixture request failed: {error}");
                }
            }
            Err(error) => eprintln!("fixture accept failed: {error}"),
        }
    }
    Ok(())
}
