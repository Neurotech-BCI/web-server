use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder};
use std::sync::Arc;
use std::sync::Mutex;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::io::Read;
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

#[derive(Default)]
struct RecordingState {
    active:     bool,
    start_idx:  usize,
}

type SharedState = std::sync::Arc<std::sync::Mutex<RecordingState>>;

// ---------- helpers ----------------------------------------------------------
fn packet_path(idx: usize) -> PathBuf {
    Path::new(CSV_DIR).join(format!("{idx}.csv"))
}

// ---------- handlers ---------------------------------------------------------

/// GET /data
/// – If a demo is running, stream packets **in order** starting at the first
///   one recorded.  Each call returns exactly one CSV packet.
/// – If there is no active demo, respond 204 (no content).
/// – If the viewer has already consumed every packet available so far,
///   also respond 204 so the front‑end can simply poll again later.
///
/// On success: 200 OK + `text/csv` body
#[get("/data")]
async fn get_data(state: web::Data<SharedState>) -> impl Responder {
    let st = state.lock().unwrap();
    if !st.active {
        // No active demo — nothing to stream
        return HttpResponse::NoContent().finish();
    }

    let idx_to_send = CURRENT_SENDING.load(Ordering::SeqCst);
    let latest_idx  = CURRENT_RECEIVING.load(Ordering::SeqCst);

    if idx_to_send >= latest_idx {
        // We’ve caught up; viewer should poll again later
        return HttpResponse::NoContent().finish();
    }

    // Build path like csv_packets/123.csv
    let path = packet_path(idx_to_send);
    drop(st); // release lock before I/O

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            CURRENT_SENDING.fetch_add(1, Ordering::SeqCst); // advance pointer
            HttpResponse::Ok()
                .content_type("text/csv")
                .body(content)
        }
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Failed to read packet {idx_to_send}: {e}")),
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

/// POST /demo/start   – begin a recording window
#[post("/demo/start")]
async fn start_demo(data: web::Data<SharedState>) -> impl Responder {
    let mut st = data.lock().unwrap();
    if st.active {
        return HttpResponse::BadRequest().body("Demo already running");
    }
    st.active    = true;
    st.start_idx = CURRENT_RECEIVING.load(Ordering::SeqCst);

    // Reset the “next packet to send” pointer for live‑viz
    CURRENT_SENDING.store(st.start_idx, Ordering::SeqCst);

    HttpResponse::Ok().body("Recording started")
}

/// POST /demo/stop    – end window, bundle packets, run inference, return JSON
#[post("/demo/stop")]
async fn stop_demo(data: web::Data<SharedState>) -> impl Responder {
    // ----- grab & reset state ----------------------------------------------
    let (start_idx, end_idx) = {
        let mut st = data.lock().unwrap();
        if !st.active {
            return HttpResponse::BadRequest().body("No demo running");
        }
        st.active = false;
        let end = CURRENT_RECEIVING.load(Ordering::SeqCst);
        (st.start_idx, end)
    };

    // ----- concatenate packets ---------------------------------------------
    use std::fs::File;
    use std::io::{BufReader};

    let mut combined = String::new();
    for idx in start_idx..end_idx {
        let path = packet_path(idx);
        if let Ok(file) = File::open(&path) {
            let mut buf = String::new();
            BufReader::new(file).read_to_string(&mut buf).ok();
            if idx != start_idx {
                // drop header
                if let Some(pos) = buf.find('\n') { buf = buf[pos + 1..].to_string() }
            }
            combined.push_str(&buf);
            if !combined.ends_with('\n') { combined.push('\n'); }
        }
    }

    // ----- POST to inference service ---------------------------------------
    let client = reqwest::Client::new();
    let resp = match client
        .post(INFERENCE_ENDPOINT)
        .json(&serde_json::json!({ "csv_data": combined }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return HttpResponse::InternalServerError()
                    .body(format!("Failed to reach inference: {e}")),
    };

    let status = resp.status();
    let body   = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return HttpResponse::InternalServerError()
               .body(format!("Inference error: {status} – {body}"));
    }
    // try JSON first, fallback to plain text
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(json) => HttpResponse::Ok().json(json),
        Err(_)   => HttpResponse::Ok().body(body),
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

    let state: SharedState = Arc::new(Mutex::new(RecordingState::default()));
    let state_data = web::Data::new(state);

    println!("🚀  CSV server listening on 0.0.0.0:6000");
    HttpServer::new(move || {
        App::new()
            .app_data(web::PayloadConfig::new(20 * 1024 * 1024)) // 20 MB upload cap
            .app_data(state_data.clone())
            .service(upload_csv)
            .service(inference)
            .service(get_data)
            .service(start_demo)
            .service(stop_demo)
            .service(health)
    })
    .bind(("0.0.0.0", 6000))?
    .run()
    .await
}