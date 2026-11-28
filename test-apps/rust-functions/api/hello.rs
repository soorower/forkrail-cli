//! A Vercel Rust function (vercel_runtime 2), unchanged.
use serde_json::json;
use vercel_runtime::{Error, Request, Response, ResponseBody, run, service_fn};

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}

async fn handler(req: Request) -> Result<Response<ResponseBody>, Error> {
    println!("hello {}", req.uri());
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(json!({"function": "api/hello.rs", "path": req.uri().path(), "arch": std::env::consts::ARCH}).to_string().into())?)
}
