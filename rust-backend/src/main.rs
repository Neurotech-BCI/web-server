use actix_web::{get, post, App, HttpServer, HttpResponse, Responder, web};
use std::fs::OpenOptions;          // ← bring OpenOptions into scope
use std::io::{Write, BufWriter};
use std::path::Path;
use serde_json::Value;

// for appending to    let mut file = Vec::new();

const CSV_FILE_PATH: &str = "data.csv";

// for sending the test request
const TEST_CSV_PATH: &str = "/root/InferenceAPI/test_data/test.csv";
const INFERENCE_ENDPOINT: &str = "http://127.0.0.1:8000/inference";

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

// just used to check the server status
#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("OK")
}

#[get("/inference")]
async fn inference() -> impl Responder {
    use std::fs;
    use reqwest::blocking::Client;
    use serde_json::json;

    // Read test CSV content as a string
    let csv_data = match fs::read_to_string(TEST_CSV_PATH) {
        Ok(content) => content,
        Err(e) => return HttpResponse::InternalServerError().body(format!("Failed to read test CSV: {}", e)),
    };

    // Create JSON body
    let payload = json!({ "csv_data": csv_data });

    // Send POST request
    let client = Client::new();
    let response = match client
        .post(INFERENCE_ENDPOINT)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
    {
        Ok(resp) => resp,
        Err(e) => return HttpResponse::InternalServerError().body(format!("Failed to send request: {}", e)),
    };

    // Process response
    if response.status().is_success() {
        let response_body: String = match response.text() {
            Ok(text) => text,
            Err(e) => return HttpResponse::InternalServerError().body(format!("Failed to read response: {}", e)),
        };        

        let json_response: Value = match serde_json::from_str(&response_body) {
            Ok(val) => val,
            Err(e) => return HttpResponse::InternalServerError().body(format!("Failed to parse JSON: {}", e)),
        };

        println!("Response: {}", json_response);
        HttpResponse::Ok().json(json_response)
    } else {
        HttpResponse::InternalServerError().body("Failed to send inference request")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| {
        App::new()
            .service(upload_csv)
            .service(health)
            .service(inference)
            .app_data(web::PayloadConfig::new(20 * 1024 * 1024)) // 20 MB limit
    })
    .bind(("0.0.0.0", 6000))?
    .run()
    .await
}
