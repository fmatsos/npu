//! Adaptateur de protocole `openai-compatible`, opération `chat` uniquement.
//!
//! Décision d'architecture (IMPLEMENTATION.md §0) : le core embarque la
//! connaissance du **protocole** `OpenAI` (forme du corps de requête, chemin
//! d'extraction de la réponse), pas de sémantique métier. `chat` est la seule
//! opération supportée en phase 1 ; les autres opérations `OpenAI`
//! (`embeddings`, `audio_transcriptions`, ...) étendront cet adaptateur sans
//! toucher au modèle de domaine (`config.rs`).

use std::time::Duration;

use serde_json::Value;

/// Délai maximal accordé à une requête `chat` avant échec.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Nombre maximal de caractères du corps de réponse inclus dans un message
/// d'erreur, pour rester diagnosticable sans noyer stderr.
const ERROR_BODY_TRUNCATE_AT: usize = 500;

/// Exécute l'opération `chat` du `model` donné auprès de `backend`, avec `prompt`.
pub fn chat(
    backend: &crate::config::Backend,
    model: &crate::config::Model,
    prompt: &str,
) -> crate::Result<String> {
    let operation = backend.operations.get(&model.operation).ok_or_else(|| {
        crate::Error::Config(format!(
            "le backend « {} » n'expose pas l'opération « {} » (opérations disponibles : {})",
            backend.id,
            model.operation,
            crate::error::format_available(backend.operations.keys())
        ))
    })?;

    let url = join_url(&backend.base_url, &operation.path);
    let body = build_chat_request(&model.model, prompt, &model.generation);

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        // Un statut non-2xx doit rester lisible : on veut son corps dans le
        // message d'erreur, pas une erreur ureq opaque.
        .http_status_as_error(false)
        .build()
        .new_agent();

    let mut response = agent.post(&url).send_json(&body).map_err(|err| {
        crate::Error::Backend(format!(
            "requête vers le backend « {} » ({url}) échouée : {err}",
            backend.id
        ))
    })?;

    let status = response.status();
    let response_text = response.body_mut().read_to_string().map_err(|err| {
        crate::Error::Backend(format!(
            "lecture de la réponse du backend « {} » ({url}) échouée : {err}",
            backend.id
        ))
    })?;

    if !status.is_success() {
        return Err(crate::Error::Backend(format!(
            "le backend « {} » ({url}) a répondu avec le statut {status} : {}",
            backend.id,
            truncate(&response_text, ERROR_BODY_TRUNCATE_AT)
        )));
    }

    let response_json: Value = serde_json::from_str(&response_text).map_err(|err| {
        crate::Error::Backend(format!(
            "réponse du backend « {} » ({url}) illisible en JSON : {err} ; corps reçu : {}",
            backend.id,
            truncate(&response_text, ERROR_BODY_TRUNCATE_AT)
        ))
    })?;

    extract_chat_content(&response_json).ok_or_else(|| {
        crate::Error::Backend(format!(
            "réponse du backend « {} » ({url}) sans contenu exploitable (attendu \
             choices[0].message.content) ; corps reçu : {}",
            backend.id,
            truncate(&response_text, ERROR_BODY_TRUNCATE_AT)
        ))
    })
}

/// Joint une URL de base et un chemin d'opération sans doubler ni perdre le `/`.
fn join_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Construit le corps de requête `chat/completions` au format `OpenAI`.
///
/// `temperature` et `max_tokens` ne sont insérés que s'ils valent `Some` :
/// aucune valeur `null` n'est sérialisée pour un champ absent.
fn build_chat_request(model: &str, prompt: &str, generation: &crate::config::Generation) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": prompt }
        ]
    });

    if let Value::Object(map) = &mut body {
        if let Some(temperature) = generation.temperature {
            // Passer par `f64` directement (`Value::from(f32)`) réintroduit le
            // bruit binaire du f32 (ex. 0.7 -> 0.699999988079071). Repasser par
            // la représentation textuelle la plus courte du f32 (celle que
            // `Display`/`ToString` produisent) donne un f64 qui affiche la
            // même valeur que celle écrite en config.
            if let Some(number) = serde_json::Number::from_f64(f64_from_f32_text(temperature)) {
                map.insert("temperature".to_string(), Value::Number(number));
            }
        }
        if let Some(max_tokens) = generation.max_tokens {
            map.insert("max_tokens".to_string(), Value::from(max_tokens));
        }
    }

    body
}

/// Convertit un `f32` en `f64` via sa représentation textuelle la plus courte,
/// pour éviter d'exposer le bruit binaire introduit par un élargissement direct
/// `f32 -> f64` (`0.7_f32 as f64` != `0.7_f64`).
fn f64_from_f32_text(value: f32) -> f64 {
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(value))
}

/// Extrait `choices[0].message.content` d'une réponse `chat/completions`.
fn extract_chat_content(response: &Value) -> Option<String> {
    response
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// Tronque `s` à `max_chars` caractères, en respectant les frontières UTF-8.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]) ; même
// convention que dans lib.rs/command.rs/config.rs/input.rs.
mod tests {
    use super::*;
    use crate::config::{Generation, Operation};

    #[test]
    fn join_url_without_trailing_slash_on_base() {
        assert_eq!(
            join_url("http://127.0.0.1:8000", "/v3/chat/completions"),
            "http://127.0.0.1:8000/v3/chat/completions"
        );
    }

    #[test]
    fn join_url_with_trailing_slash_on_base() {
        assert_eq!(
            join_url("http://127.0.0.1:8000/", "/v3/chat/completions"),
            "http://127.0.0.1:8000/v3/chat/completions"
        );
    }

    #[test]
    fn join_url_with_path_missing_leading_slash() {
        assert_eq!(
            join_url("http://127.0.0.1:8000", "v3/chat/completions"),
            "http://127.0.0.1:8000/v3/chat/completions"
        );
    }

    #[test]
    fn build_chat_request_without_generation_options() {
        let generation = Generation {
            temperature: None,
            max_tokens: None,
        };
        let body = build_chat_request("qwen-2.5-1.5b", "bonjour", &generation);

        assert_eq!(body["model"], "qwen-2.5-1.5b");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "bonjour");
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn build_chat_request_with_generation_options() {
        let generation = Generation {
            temperature: Some(0.0),
            max_tokens: Some(512),
        };
        let body = build_chat_request("qwen-2.5-1.5b", "bonjour", &generation);

        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 512);
    }

    #[test]
    fn build_chat_request_temperature_has_no_f32_widening_noise() {
        // `0.7_f32 as f64` != `0.7_f64` (bruit binaire) : le corps émis doit
        // afficher la même valeur que celle écrite en config, pas sa version
        // élargie bruitée (ex. 0.699999988079071).
        let generation = Generation {
            temperature: Some(0.7),
            max_tokens: None,
        };
        let body = build_chat_request("qwen-2.5-1.5b", "bonjour", &generation);

        assert_eq!(body["temperature"].to_string(), "0.7");
    }

    #[test]
    fn extract_chat_content_nominal() {
        let response = serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": "réponse" } }
            ]
        });
        assert_eq!(extract_chat_content(&response), Some("réponse".to_string()));
    }

    #[test]
    fn extract_chat_content_missing_choices() {
        let response = serde_json::json!({ "error": "boom" });
        assert_eq!(extract_chat_content(&response), None);
    }

    #[test]
    fn extract_chat_content_empty_choices() {
        let response = serde_json::json!({ "choices": [] });
        assert_eq!(extract_chat_content(&response), None);
    }

    #[test]
    fn truncate_keeps_short_string_unchanged() {
        assert_eq!(truncate("court", 500), "court");
    }

    #[test]
    fn truncate_cuts_long_string() {
        let long = "a".repeat(600);
        let truncated = truncate(&long, 500);
        assert_eq!(truncated.chars().count(), 501); // 500 + '…'
        assert!(truncated.ends_with('…'));
    }

    /// Test d'intégration contre un listener HTTP bouchonné (cf.
    /// IMPLEMENTATION.md §4) : couvre `chat()` de bout en bout, sans dépendance
    /// supplémentaire (juste `std::net`/`std::thread`). Aucun des autres tests
    /// de ce module n'exerce `chat()` elle-même, seulement ses fonctions
    /// privées : c'était le trou de couverture le plus notable du module.
    #[test]
    fn chat_end_to_end_against_stubbed_http_server() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind du listener bouchonné");
        let addr = listener.local_addr().expect("adresse locale du listener");

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("acceptation de la connexion");
            let mut reader = BufReader::new(stream.try_clone().expect("clone du flux TCP"));

            // Draine les en-têtes de la requête jusqu'à la ligne vide, puis le
            // corps annoncé par Content-Length ; son contenu exact n'a pas
            // besoin d'être vérifié pour ce test de bout en bout.
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .expect("lecture d'une ligne d'en-tête");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader
                .read_exact(&mut body)
                .expect("lecture du corps de la requête");

            let response_body =
                r#"{"choices":[{"message":{"role":"assistant","content":"stubbed reply"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let mut stream = stream;
            stream
                .write_all(response.as_bytes())
                .expect("écriture de la réponse bouchonnée");
        });

        let backend = crate::config::Backend {
            id: "stub".to_string(),
            base_url: format!("http://{addr}"),
            kind: "openai-compatible".to_string(),
            operations: [(
                "chat".to_string(),
                Operation {
                    method: "POST".to_string(),
                    path: "/v1/chat/completions".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        };
        let model = crate::config::Model {
            id: "test-model".to_string(),
            backend: "stub".to_string(),
            operation: "chat".to_string(),
            model: "test-model".to_string(),
            generation: Generation::default(),
        };

        let result = chat(&backend, &model, "bonjour")
            .expect("chat() doit réussir contre le listener bouchonné");
        assert_eq!(result, "stubbed reply");

        server
            .join()
            .expect("le thread serveur ne doit pas paniquer");
    }
}
