use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder, middleware::Logger};
use log::error;
use parking_lot::Mutex;
use reqwest::Client;
use log::info;
use chrono::Utc;
use serde_json::Value;
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
};

/// Path for your local test CSV
const TEST_CSV_PATH: &str = "/root/InferenceAPI/test_data/test_1.csv";
/// Where your inference microservice lives
const INFERENCE_ENDPOINT: &str = "http://127.0.0.1:8000/inference";
/// Max packets per demo run
const MAX_SAMPLES: usize = 120;

// debug flag
const DEBUG: bool = false;

/// In‑memory CSV accumulator
#[derive(Default)]
struct CsvCache {
    buf: String,
    header_seen: bool,
    samples: usize,
    last_ts: Option<f64>,
}

impl CsvCache {
    fn reset(&mut self) {
        self.buf.clear();
        self.header_seen = false;
        self.samples = 0;
        self.last_ts = None;
    }

    fn push_packet(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if self.samples >= MAX_SAMPLES {
            return Err("demo buffer full");
        }

        let text   = std::str::from_utf8(bytes).map_err(|_| "CSV must be UTF-8")?;
        let mut it = text.lines();

        // ─── 1. header handling ───────────────────────────────────────────────
        if !self.header_seen {
            if let Some(hdr) = it.next() {
                self.buf.push_str(hdr);
                self.buf.push('\n');
            }
            self.header_seen = true;
        } else {
            // skip header of every *subsequent* packet
            let _ = it.next();
        }

        // ─── 2. append rows, remembering whether we kept at least one ────────
        let mut accepted_any = false;

        for line in it {
            let ts_str = line.split(',').next().unwrap_or("").trim_matches('"');

            if let Ok(ts) = ts_str.parse::<f64>() {
                if self.last_ts.map_or(false, |prev| ts <= prev) {
                    continue;                      // duplicate or out-of-order
                }
                self.last_ts = Some(ts);
            }

            self.buf.push_str(line);
            self.buf.push('\n');
            accepted_any = true;
        }

        // ─── 3. bump the packet counter *only* when we really added data ─────
        if accepted_any {
            self.samples += 1;
        }

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

    let mut csv = {
        let guard = app.cache.lock();
        guard.buf.clone()
    };

    if csv.trim().is_empty() {
        return HttpResponse::Ok().body("No data collected");
    }

    // save the CSV to a file
    let filename = format!("/tmp/demo_{}.csv", chrono::Utc::now().format("%Y%m%d_%H%M%S"));
    if let Err(e) = fs::write(&filename, &csv) {
        error!("Failed to write CSV file: {e}");
        return HttpResponse::InternalServerError()
            .body(format!("Failed to write CSV file: {e}"));
    }
    info!("CSV saved to {filename}");

    if DEBUG {
        // use the test data
        csv = fs::read_to_string(TEST_CSV_PATH).unwrap_or_default();
    }

    // send the CSV to the inference service
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
