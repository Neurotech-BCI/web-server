use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder, middleware::Logger};
use log::error;
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::Value;
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
};

/// Path for your local test CSV
const TEST_CSV_PATH: &str = "/root/InferenceAPI/test_data/test.csv";
/// Where your inference microservice lives
const INFERENCE_ENDPOINT: &str = "http://127.0.0.1:8000/inference";
/// Max packets per demo run
const MAX_SAMPLES: usize = 5;

/// In‑memory CSV accumulator
#[derive(Default)]
struct CsvCache {
    buf: String,
    header_seen: bool,
    samples: usize,
}

impl CsvCache {
    fn reset(&mut self) {
        self.buf.clear();
        self.header_seen = false;
        self.samples = 0;
    }

    fn push_packet(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if self.samples >= MAX_SAMPLES {
            return Err("demo buffer full");
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "CSV must be UTF‑8")?;
        let mut lines = text.lines();

        // keep header only once
        if !self.header_seen {
            if let Some(h) = lines.next() {
                self.buf.push_str(h);
                self.buf.push('\n');
            }
            self.header_seen = true;
        } else {
            // skip this packet's header
            let _ = lines.next();
        }

        // append the rest
        for line in lines {
            self.buf.push_str(line);
            self.buf.push('\n');
        }

        self.samples += 1;
        Ok(())
    }
}

type SharedData = web::Data<AppState>;

/// Kubernetes/NGINX healthcheck
#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("OK")
}

/// Polling endpoint: return current CSV buffer if demo is active
#[get("/data")]
async fn get_data(app: SharedData) -> impl Responder {
    // lock‑free check
    if !app.active.load(Ordering::Acquire) {
        return HttpResponse::NoContent().finish();
    }

    // snapshot buffer + sample count
    let (csv, samples) = {
        let guard = app.cache.lock();
        (guard.buf.clone(), guard.samples)
    };

    if csv.is_empty() {
        return HttpResponse::NoContent().finish();
    }

    // build response without temporary borrows
    let mut builder = HttpResponse::Ok();
    builder.content_type("text/csv");
    if samples >= MAX_SAMPLES {
        builder.insert_header(("X-Demo-Complete", "true"));
    }
    builder.body(csv)
}

/// Raspberry Pi streams CSV packets here
#[post("/upload")]
async fn upload_csv(body: web::Bytes, app: SharedData) -> impl Responder {
    if !app.active.load(Ordering::Acquire) {
        return HttpResponse::BadRequest().body("No active demo");
    }

    let mut guard = app.cache.lock();
    match guard.push_packet(&body) {
        Ok(())                  => HttpResponse::Ok().body("Packet cached"),
        Err("demo buffer full") => HttpResponse::NoContent().finish(),
        Err(e)                  => HttpResponse::BadRequest().body(e),
    }
}

/// Quick inference on a static test CSV
#[get("/inference")]
async fn inference_proxy() -> impl Responder {
    let csv = match fs::read_to_string(TEST_CSV_PATH) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to read test CSV: {e}");
            return HttpResponse::InternalServerError()
                .body(format!("Failed to read test CSV: {e}"));
        }
    };

    let client = Client::new();
    let resp = match client
        .post(INFERENCE_ENDPOINT)
        .json(&serde_json::json!({ "csv_data": csv }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Inference request error: {e}");
            return HttpResponse::InternalServerError()
                .body(format!("Request error: {e}"));
        }
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return HttpResponse::InternalServerError()
            .body(format!("Inference service error: {status} – {text}"));
    }

    match serde_json::from_str::<Value>(&text) {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(_) => HttpResponse::Ok().body(text),
    }
}

/// Start a new demo run
#[post("/demo/start")]
async fn start_demo(app: SharedData) -> impl Responder {
    if app.active.swap(true, Ordering::AcqRel) {
        return HttpResponse::BadRequest().body("Demo already running");
    }
    app.cache.lock().reset();
    HttpResponse::Ok().body("Recording started")
}

/// Stop demo and run inference on collected data
#[post("/demo/stop")]
async fn stop_demo(app: SharedData) -> impl Responder {
    if !app.active.swap(false, Ordering::AcqRel) {
        return HttpResponse::BadRequest().body("No demo running");
    }

    let csv = {
        let guard = app.cache.lock();
        guard.buf.clone()
    };

    if csv.trim().is_empty() {
        return HttpResponse::Ok().body("No data collected");
    }

    let client = Client::new();
    let resp = match client
        .post(INFERENCE_ENDPOINT)
        .json(&serde_json::json!({ "csv_data": csv }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Inference request error: {e}");
            return HttpResponse::InternalServerError()
                .body(format!("Request error: {e}"));
        }
    };

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return HttpResponse::InternalServerError()
            .body(format!("Inference error: {status} – {body}"));
    }

    match serde_json::from_str::<Value>(&body) {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(_) => HttpResponse::Ok().body(body),
    }
}

// 1) Define your state
struct AppState {
    active: AtomicBool,
    cache:  Mutex<CsvCache>,
}

// 2) In main(), build it like this
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // … env_logger setup …

    // Directly wrap AppState in Data<T>
    let state = web::Data::new(AppState {
        active: AtomicBool::new(false),
        cache:  Mutex::default(),
    });

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(state.clone())
            .service(health)
            .service(start_demo)      // <‑‑ ADD THESE
            .service(stop_demo)
            .service(upload_csv)
            .service(get_data)
            .service(inference_proxy)
    })
    .bind(("0.0.0.0", 6000))?
    .run()
    .await
}
