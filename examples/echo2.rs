use std::{env, io};

use actix_http::http::HeaderValue;
use actix_http::HttpMessage;
use actix_http::{Error, HttpService, Request, Response};
use actix_server::Server;
use bytes::Bytes;
use futures::Future;
use log::info;

fn handle_request(mut req: Request) -> impl Future<Item = Response, Error = Error> {
    req.body().limit(512).from_err().and_then(|bytes: Bytes| {
        info!("request body: {:?}", bytes);
        let mut res = Response::Ok();
        res.header("x-head", HeaderValue::from_static("dummy value!"));
        Ok(res.body(bytes))
    })
}

fn main() -> io::Result<()> {
    env::set_var("RUST_LOG", "echo=info");
    env_logger::init();

    Server::build()
        .bind("echo", "127.0.0.1:8080", || {
            HttpService::build().finish(|_req: Request| handle_request(_req))
        })?
        .run()
}
