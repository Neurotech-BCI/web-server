use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

// ---------- config & globals -------------------------------------------------
const CSV_DIR: &str = "csv_packets";
const MASTER_CSV: &str = "csv_packets/MASTER.csv";

const TEST_CSV_PATH: &str = "/root/InferenceAPI/test_data/test.csv";
const INFERENCE_ENDPOINT: &str = "http://127.0.0.1:8000/inference";

static CURRENT_RECEIVING: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
static CURRENT_SENDING:   Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

// ---------- helpers ----------------------------------------------------------
fn packet_path(idx: usize) -> PathBuf {
    Path::new(CSV_DIR).join(format!("{idx}.csv"))
}

// ---------- handlers ---------------------------------------------------------
/* ---------------------------------------------------------------------------
   GET /data
   Sends csv_packets/CURRENT_SENDING.csv and increments CURRENT_SENDING
--------------------------------------------------------------------------- */
#[get("/data")]
async fn get_data() -> impl Responder {
    let idx   = CURRENT_SENDING.load(Ordering::SeqCst);
    let path  = packet_path(idx);

    if !path.exists() {
        return HttpResponse::NotFound().body(format!("Packet {idx}.csv not found"));
    }

    match fs::read_to_string(&path) {
        Ok(content) => {
            CURRENT_SENDING.fetch_add(1, Ordering::SeqCst);
            HttpResponse::Ok()
                .content_type("text/csv")
                .body(content)
        }
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Failed to read {path:?}: {e}")),
    }
}

/* ---------------------------------------------------------------------------
   POST /upload
   Stores the body as csv_packets/CURRENT_RECEIVING.csv,
   appends to csv_packets/MASTER.csv (dropping header if MASTER exists),
   then increments CURRENT_RECEIVING.
--------------------------------------------------------------------------- */
#[post("/upload")]
async fn upload_csv(body: web::Bytes) -> impl Responder {
    // ---------- decide what to append to MASTER.CSV ----------
    let master_exists = Path::new(MASTER_CSV).exists();

    let cleaned = if master_exists {
        // drop first line (header)
        match std::str::from_utf8(&body) {
            Ok(text) => text.lines().skip(1).collect::<Vec<_>>().join("\n"),
            Err(_)   => return HttpResponse::BadRequest().body("CSV must be valid UTF‑8"),
        }
    } else {
        String::from_utf8_lossy(&body).into_owned()
    };

    // ---------- write per‑packet file ----------
    let idx      = CURRENT_RECEIVING.load(Ordering::SeqCst);
    let pkt_path = packet_path(idx);

    if let Err(e) = fs::write(&pkt_path, &body) {
        eprintln!("Packet write error: {e}");
        return HttpResponse::InternalServerError().body("Failed to store packet file");
    }

    // ---------- append to MASTER ----------
    if let Err(e) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(MASTER_CSV)
        .and_then(|file| {
            let mut w = BufWriter::new(file);
            w.write_all(cleaned.as_bytes())?;
            w.write_all(b"\n")               // ensure newline after each packet
        })
    {
        eprintln!("Master write error: {e}");
        return HttpResponse::InternalServerError().body("Failed to append to MASTER.csv");
    }

    CURRENT_RECEIVING.fetch_add(1, Ordering::SeqCst);
    HttpResponse::Ok().body("CSV stored & appended")
}

/* ---------------------------------------------------------------------------
   GET /health  – simple liveness probe
--------------------------------------------------------------------------- */
#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("OK")
}

/* ---------------------------------------------------------------------------
   GET /inference
   Reads test.csv, wraps it in JSON, POSTs to the inference service,
   and returns the JSON response to the caller.
--------------------------------------------------------------------------- */
#[get("/inference")]
async fn inference() -> impl Responder {
    // read test CSV -----------------------------------------------------------
    let csv_data = match fs::read_to_string(TEST_CSV_PATH) {
        Ok(c) => c,
        Err(e) => return HttpResponse::InternalServerError()
            .body(format!("Failed to read test CSV: {e}")),
    };

    // send async request ------------------------------------------------------
    let client  = reqwest::Client::new();
    let payload = serde_json::json!({ "csv_data": csv_data });

    let resp = match client
        .post(INFERENCE_ENDPOINT)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return HttpResponse::InternalServerError()
            .body(format!("Request error: {e}")),
    };

    // bubble up inference response -------------------------------------------
    let status = resp.status();
    let text   = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return HttpResponse::InternalServerError()
            .body(format!("Inference service error: {status} – {text}"));
    }

    match serde_json::from_str::<Value>(&text) {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(_)   => HttpResponse::Ok().body(text),    // plain text fallback
    }
}

// ---------- main ------------------------------------------------------------
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // ensure working directory exists & start fresh master file ---------------
    fs::create_dir_all(CSV_DIR)?;
    let _ = fs::remove_file(MASTER_CSV); // ignore error if it didn’t exist

    // remove any existing packets
    for entry in fs::read_dir(CSV_DIR)? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("csv") {
            fs::remove_file(path)?;
        }
    }

    println!("🚀  CSV server listening on 0.0.0.0:6000");
    HttpServer::new(|| {
        App::new()
            .service(upload_csv)
            .service(inference)
            .service(get_data)
            .service(health)
            .app_data(web::PayloadConfig::new(20 * 1024 * 1024)) // 20 MB upload cap
    })
    .bind(("0.0.0.0", 6000))?
    .run()
    .await
}