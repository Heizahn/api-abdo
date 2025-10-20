use crate::auth::controller;
use crate::db::Db;
use crate::http::{request::Request, response::Response};

#[derive(Clone, Copy)]
pub struct AuthRouter;

impl AuthRouter {
    pub fn handle<DB: Db>(&self, req: &Request, db: DB) -> Option<Response> {
        match req.path.strip_prefix("/v1/auth") {
            Some(path) => match path {
                "/login" => Some(match req.method.as_str() {
                    "POST" => controller::login(req, db),
                    "OPTIONS" => Response::options_ok(),
                    _ => Response::method_not_allowed(),
                }),
                "/verify_number" => Some(match req.method.as_str() {
                    "POST" => controller::verify_number(req, db),
                    "OPTIONS" => Response::options_ok(),
                    _ => Response::method_not_allowed(),
                }),
                "/refresh" => Some(match req.method.as_str() {
                    "POST" => controller::refresh(req, db),
                    "OPTIONS" => Response::options_ok(),
                    _ => Response::method_not_allowed(),
                }),
                _ => None,
            },
            None => None,
        }
    }
}
