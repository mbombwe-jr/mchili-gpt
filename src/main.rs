use actix_web::{post, web, App, HttpResponse, HttpServer, Responder};
use reqwest::Client;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::json;

//
// ==== Incoming payload types ====
//
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReceived {
    pub device_id: String,
    pub event: String,
    pub id: String,
    pub payload: Payload,
    pub webhook_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub message: String,
    pub received_at: String,
    pub message_id: String,
    pub phone_number: String,
    pub sim_number: u8,
}

fn format_ai_response(raw: &str) -> String {
    raw.replace('\\', "") // remove any backslashes before quotes
        .replace('*', "") // remove asterisks
        .replace("\"", "") // optional: remove quotes if needed
        .replace("\\n", "\n") // convert \n into actual newlines
        .trim()
        .to_string()
}

//
// ==== DB helpers ====
//
fn open_db() -> rusqlite::Result<Connection> {
    let conn = Connection::open("chat_memory.db")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS conversations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            phone_number TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;
    Ok(conn)
}

fn append_message(
    conn: &Connection,
    phone: &str,
    role: &str,
    content: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO conversations (phone_number, role, content) VALUES (?1, ?2, ?3)",
        params![phone, role, content],
    )?;
    Ok(())
}

fn load_history(conn: &Connection, phone: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT role, content FROM conversations WHERE phone_number = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([phone], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut history = Vec::new();
    for r in rows {
        history.push(r?);
    }
    Ok(history)
}

//
// ==== Mistral AI Chat call ====
//
async fn call_mistral(
    http: &Client,
    user_input: &str,
    history: &[(String, String)],
) -> Result<String, String> {
    // Hardcoded API key and model
    let api_key = "YK0W7952T7Hg3T8GIXz36zEBOng222N3";
    let model = "mistral-large-latest";
    let system_prompt = "You are Mchili a concise, helpful assistant. Keep replies short and actionable. do not reply in more than 4 sentenses and remember you are made by zoofam company and you are called mchili";

    // Build message list
    let mut messages = vec![json!({"role": "system", "content": system_prompt})];

    for (role, content) in history {
        let role = match role.as_str() {
            "human" => "user",
            "ai" => "assistant",
            _ => "user",
        };
        messages.push(json!({"role": role, "content": content}));
    }

    messages.push(json!({"role": "user", "content": user_input}));

    let payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": 512,
        "temperature": 0.7
    });

    let resp = http
        .post("https://api.mistral.ai/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Mistral request error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Mistral API bad status {status}: {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Mistral parse error: {e}"))?;
    let ai_reply = data["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("Sorry, I couldn't generate a reply.")
        .to_string();

    Ok(ai_reply)
}

//
// ==== Send SMS via sms-gate ====
//
async fn send_sms_gate(http: &Client, text: &str, phone: &str) -> Result<(u16, String), String> {
    let username = "5Q15LJ"; // Hardcoded
    let password = "0dyplbr43ptuyf"; // Hardcoded
    let formated_text = format_ai_response(text);
    let body = json!({
        "textMessage": { "text": formated_text },
        "phoneNumbers": [phone]
    });

    let resp = http
        .post("https://api.sms-gate.app/3rdparty/v1/message")
        .basic_auth(username, Some(password))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("SMS Gate send error: {e}"))?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

//
// ==== Webhook handler ====
//
#[post("/message-received")]
async fn message_received(body: web::Json<MessageReceived>) -> impl Responder {
    let msg = body.into_inner();
    let phone = msg.payload.phone_number.trim().to_string();
    let human_text = msg.payload.message.trim().to_string();

    // Open DB and store human message
    let conn = match open_db() {
        Ok(c) => c,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .json(json!({"error": format!("DB open error: {e}")}))
        }
    };

    if let Err(e) = append_message(&conn, &phone, "human", &human_text) {
        return HttpResponse::InternalServerError()
            .json(json!({"error": format!("DB insert error: {e}")}));
    }

    // Load history
    let history = match load_history(&conn, &phone) {
        Ok(h) => h,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .json(json!({"error": format!("DB load error: {e}")}))
        }
    };

    let http = Client::new();

    // Call Mistral
    let ai_reply = match call_mistral(&http, &human_text, &history).await {
        Ok(r) => r,
        Err(e) => return HttpResponse::BadGateway().json(json!({"error": e})),
    };

    // Save AI reply
    if let Err(e) = append_message(&conn, &phone, "ai", &ai_reply) {
        return HttpResponse::InternalServerError()
            .json(json!({"error": format!("DB insert AI error: {e}")}));
    }

    // Send reply via SMS
    let (sms_status, sms_response) = match send_sms_gate(&http, &ai_reply, &phone).await {
        Ok(x) => x,
        Err(e) => (502, e),
    };

    HttpResponse::Ok().json(json!({
        "to": phone,
        "human": human_text,
        "ai": ai_reply,
        "sms_status": sms_status,
        "sms_response": sms_response
    }))
}

//
// ==== Run server ====
//
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| App::new().service(message_received))
        .bind(("127.0.0.1", 3000))?
        .run()
        .await
}
