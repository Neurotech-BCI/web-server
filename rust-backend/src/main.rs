use actix_web::{get, post, App, HttpServer, HttpResponse, Responder, web};
use std::fs::OpenOptions;          // ← bring OpenOptions into scope
use std::io::{Write, BufWriter};
use std::path::Path;

const CSV_FILE_PATH: &str = "data.csv";

#[post("/data")]
async fn upload_csv(body: web::Bytes) -> impl Responder {
    let file_exists = Path::new(CSV_FILE_PATH).exists();

    // If the file exists, drop the header row
    let data_to_write = if file_exists {
        match std::str::from_utf8(&body) {
            Ok(text) => {
                let mut lines = text.lines();
                lines.next();                       // skip header
                lines.collect::<Vec<_>>().join("\n").into_bytes()
            }
            Err(_) => {
                eprintln!("Invalid UTF‑8 in CSV body");
                return HttpResponse::BadRequest().body("CSV must be valid UTF‑8");
            }
        }
    } else {
        body.to_vec()
    };

    // Open (or create) the file in append mode
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(CSV_FILE_PATH)
    {
        Ok(file) => {
            let mut writer = BufWriter::new(file);
            if let Err(e) = writer.write_all(&data_to_write) {
                eprintln!("Failed to write CSV: {e}");
                return HttpResponse::InternalServerError().body("Failed to write data");
            }
            let _ = writer.write_all(b"\n");        // ensure newline
            HttpResponse::Ok().body("CSV data appended")
        }
        Err(e) => {
            eprintln!("Failed to open CSV file: {e}");
            HttpResponse::InternalServerError().body("Could not open or create CSV file")
        }
    }
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("OK")
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| {
        App::new()
            .service(upload_csv)
            .service(health)
            .app_data(web::PayloadConfig::new(20 * 1024 * 1024)) // 20 MB limit
    })
    .bind(("0.0.0.0", 6000))?
    .run()
    .await
}
