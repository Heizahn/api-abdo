use crate::db::Db;
use crate::http::server::Handler;
use crate::http::{request::Request, response::Response};

use crate::auth::router::AuthRouter;
use crate::profile::router::ProfileRouter;

#[derive(Clone, Copy)]
pub struct AppRouter;

impl<DB: Db> Handler<DB> for AppRouter {
    fn handle(&self, req: &Request, db: DB) -> Response {
        if !req.path.starts_with("/v1/") {
            return Response::not_found();
        }

        if let Some(resp) = AuthRouter.handle(req, db.clone()) {
            return resp;
        }

        if let Some(resp) = ProfileRouter.handle(req, db.clone()) {
            return resp;
        }

        if req.method.to_string() == "OPTIONS" {
            Response::options_ok()
        } else {
            Response::not_found()
        }
    }
}
