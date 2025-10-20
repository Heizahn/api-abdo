use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use super::request::{Request, parse_request};
use super::response::Response;

pub trait Handler<DB: Send + 'static + Clone>: Send + Sync + 'static {
    fn handle(&self, req: &Request, db: DB) -> Response;
}

pub struct HttpServer<DB: Send + 'static + Clone, H: Handler<DB>> {
    addr: String,
    handler: Arc<H>,
    db: DB,
}

impl<DB: Send + 'static + Clone, H: Handler<DB>> HttpServer<DB, H> {
    pub fn new(addr: String, handler: H, db: DB) -> Self {
        Self {
            addr,
            handler: Arc::new(handler),
            db,
        }
    }

    pub fn run(&self) {
        let listener = TcpListener::bind(&self.addr).expect("bind");
        let handler = self.handler.clone();
        let db = self.db.clone();

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let handler = handler.clone();
                    let db = db.clone();
                    thread::spawn(move || handle_client(stream, handler, db));
                }
                Err(e) => eprintln!("Accept error: {e}"),
            }
        }
    }
}

fn handle_client<DB: Send + 'static + Clone, H: Handler<DB>>(
    mut stream: TcpStream,
    handler: Arc<H>,
    db: DB,
) {
    match parse_request(&stream) {
        Ok(req) => {
            // Soportar preflight simple:
            if req.method == "OPTIONS" {
                let resp = Response::options_ok();
                let _ = stream.write_all(&resp.to_bytes());
                return;
            }
            let resp = handler.handle(&req, db);
            let _ = stream.write_all(&resp.to_bytes());
        }
        Err(_) => {
            let resp = Response::not_found();
            let _ = stream.write_all(&resp.to_bytes());
        }
    }
}
