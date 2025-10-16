use crate::auth::controller;
use crate::db::Db;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::http::server::Handler;

#[derive(Clone, Copy)]
pub struct AppRouter;

impl<DB: Db> Handler<DB> for AppRouter {
    fn handle(&self, req: Request, db: DB) -> Response {
        match req.path.as_str() {
            "/auth/login" => match req.method.as_str() {
                "POST" => controller::login(req, db),
                "OPTIONS" => Response::options_ok(),
                _ => Response::method_not_allowed(),
            },

            "/auth/refresh" => match req.method.as_str() {
                "POST" => controller::refresh(req, db),
                "OPTIONS" => Response::options_ok(),
                _ => Response::method_not_allowed(),
            },

            _ => match req.method.as_str() {
                "OPTIONS" => Response::options_ok(),
                _ => Response::not_found(),
            },
        }
    }
}
