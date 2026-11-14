//! Versand von Benachrichtigungen über ntfy (https://ntfy.sh).

use crate::storage::Settings;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Message<'a> {
    pub title: &'a str,
    pub message: &'a str,
    pub click: Option<&'a str>,
    pub tags: &'a [&'a str],
}

/// Sendet eine Nachricht und liefert die Nachrichten-ID, die der ntfy-Server vergeben hat.
pub async fn send(client: &reqwest::Client, settings: &Settings, msg: Message<'_>) -> Result<String, String> {
    let server = if settings.ntfy_server.trim().is_empty() { "https://ntfy.sh" } else { settings.ntfy_server.trim() };
    let server = server.trim_end_matches('/');
    let topic = settings.ntfy_topic.trim();
    if topic.is_empty() {
        return Err("Kein ntfy-Topic konfiguriert".into());
    }
    let mut payload = json!({
        "topic": topic,
        "title": msg.title,
        "message": msg.message,
        "priority": if settings.ntfy_priority > 0 { settings.ntfy_priority } else { 4 },
        "tags": msg.tags,
    });
    if let Some(click) = msg.click.filter(|c| !c.is_empty()) {
        payload["click"] = json!(click);
        payload["actions"] = json!([{"action": "view", "label": "Zum Angebot", "url": click}]);
    }
    // JSON-Publishing an die Server-Root erlaubt Umlaute in Titel und Text
    let mut request = client.post(server).json(&payload).timeout(Duration::from_secs(20));
    if !settings.ntfy_token.is_empty() {
        request = request.bearer_auth(&settings.ntfy_token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if status == 401 || status == 403 {
        return Err(format!("ntfy lehnt ab (HTTP {}) – Access-Token oder Schreibrecht auf Topic „{topic}“ prüfen", status.as_u16()));
    }
    if !status.is_success() {
        return Err(format!("ntfy antwortet mit HTTP {}", status.as_u16()));
    }
    let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string();
    // Nur eine echte ntfy-Antwort enthält eine Nachrichten-ID. Ein 200 von etwas anderem
    // (z. B. Reverse-Proxy, falsche Server-URL) würde sonst als Erfolg durchgehen.
    let data: Option<Value> = response.json().await.ok();
    let id = data.as_ref().and_then(|d| d.get("id")).and_then(Value::as_str).filter(|id| !id.is_empty());
    let Some(id) = id else {
        return Err(format!(
            "Antwort von {server} ist keine ntfy-Antwort (HTTP {}, {content_type}) – Server-URL prüfen",
            status.as_u16()
        ));
    };
    if let Some(answered) = data.as_ref().and_then(|d| d.get("topic")).and_then(Value::as_str) {
        if answered != topic {
            return Err(format!("ntfy hat die Nachricht unter Topic „{answered}“ statt „{topic}“ angenommen"));
        }
    }
    crate::info!("ntfy: Nachricht {id} an {server}/{topic} angenommen");
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn settings(server: String) -> Settings {
        Settings { ntfy_server: server, ntfy_topic: "demo".into(), ..Default::default() }
    }

    fn msg() -> Message<'static> {
        Message { title: "t", message: "m", click: Some("https://x"), tags: &["a"] }
    }

    #[tokio::test]
    async fn accepts_real_ntfy_answer_and_rejects_others() {
        let client = reqwest::Client::new();
        let ok = serve(Router::new().route("/", post(|Json(v): Json<Value>| async move {
            Json(json!({"id": "abc123", "topic": v["topic"], "title": v["title"]}))
        }))).await;
        assert_eq!(send(&client, &settings(ok), msg()).await.unwrap(), "abc123");

        // 200 ohne ntfy-Antwort (z. B. Reverse-Proxy) ist kein Erfolg
        let proxy = serve(Router::new().route("/", post(|| async { "<html>hi</html>" }))).await;
        let err = send(&client, &settings(proxy), msg()).await.unwrap_err();
        assert!(err.contains("keine ntfy-Antwort"), "{err}");

        // Falsches Topic in der Antwort
        let wrong = serve(Router::new().route("/", post(|| async { Json(json!({"id": "x", "topic": "other"})) }))).await;
        assert!(send(&client, &settings(wrong), msg()).await.unwrap_err().contains("other"));

        let denied = serve(Router::new().route("/", post(|| async { (axum::http::StatusCode::FORBIDDEN, "no") }))).await;
        assert!(send(&client, &settings(denied), msg()).await.unwrap_err().contains("HTTP 403"));

        let no_topic = Settings { ntfy_topic: " ".into(), ..Default::default() };
        assert!(send(&client, &no_topic, msg()).await.is_err());
    }
}
