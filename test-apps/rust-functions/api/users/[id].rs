//! A dynamic segment: Vercel puts it in the query as `id`.
use serde_json::json;
use vercel_runtime::{Error, Request, Response, ResponseBody, run, service_fn};

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}

async fn handler(req: Request) -> Result<Response<ResponseBody>, Error> {
    let id = req.uri().query().unwrap_or("").split('&').find_map(|p| p.strip_prefix("id=")).unwrap_or("").to_owned();
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(json!({"function": "api/users/[id].rs", "id": id}).to_string().into())?)
}
