use actix_web::{post, web, App, HttpResponse, HttpServer, Responder};
use dotenvy::dotenv;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::mysql::MySqlPool;
use std::env;

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
    pub sim_number: u32, // safer than u8
    pub thinking: Option<Thinking>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub kind: String,
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
async fn get_db_pool() -> Result<MySqlPool, sqlx::Error> {
    dotenvy::dotenv().ok();
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set in .env file");
    MySqlPool::connect(&database_url).await
}

async fn append_message(
    pool: &MySqlPool,
    phone: &str,
    role: &str,
    content: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO conversations (phone_number, role, content) VALUES (?, ?, ?)",
        phone,
        role,
        content
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_history(pool: &MySqlPool, phone: &str) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows = sqlx::query!(
        "SELECT role, content FROM conversations WHERE phone_number = ? ORDER BY id ASC",
        phone
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| (row.role, row.content))
        .collect())
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
    let api_key = "460a5485fea14dbbb4ad08142d87d206.vN3SyerFVKWkj0jP";
    let model = "GLM-4.5-Flash";
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
        "temperature": 0.7,
        "thinking": {
          "type" : "disabled"
        },
    });

    let resp = http
        .post("https://api.z.ai/api/paas/v4/chat/completions")
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Mistral request error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Zai API bad status {status}: {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("z.ai parse error: {e}"))?;
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

    // Get database pool and store human message
    let pool = match get_db_pool().await {
        Ok(p) => p,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .json(json!({"error": format!("DB connection error: {e}")}))
        }
    };

    if let Err(e) = append_message(&pool, &phone, "human", &human_text).await {
        return HttpResponse::InternalServerError()
            .json(json!({"error": format!("DB insert error: {e}")}));
    }

    // Load history
    let history = match load_history(&pool, &phone).await {
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
    if let Err(e) = append_message(&pool, &phone, "ai", &ai_reply).await {
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
        .bind(("127.0.0.1", 3061))?
        .run()
        .await
}
