use crate::db::Db;
use crate::http::{request::Request, response::Response};

#[derive(Clone, Copy)]
pub struct ProfileRouter;

impl ProfileRouter {
    pub fn handle<DB: Db>(&self, req: &Request, db: DB) -> Option<Response> {
        match req.path.strip_prefix("/v1/profile") {
            Some(path) => match path {
                "/me" => Some(match req.method.as_str() {
                    "GET" => Response::json(200, r#"{"me":"todo"}"#),
                    "OPTIONS" => Response::options_ok(),
                    _ => Response::method_not_allowed(),
                }),
                _ => None,
            },
            None => None,
        }
    }
}
