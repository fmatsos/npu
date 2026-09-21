//! Vérification de bout en bout du contrat de sortie (phase 4,
//! npu-cli-spec.md §15) — le pipeline COMPLET, pas les unités de `output.rs`
//! ou `command.rs` prises isolément.
//!
//! Chaque test :
//! 1. monte un faux backend HTTP local (`std::net::TcpListener` dans un
//!    thread, même idiome que
//!    `backend::tests::chat_end_to_end_against_stubbed_http_server`) qui
//!    répond une réponse `chat/completions` fixée ;
//! 2. écrit un scope `.npu/` temporaire sous `target/` (backend, modèle et
//!    commande) pointant sur ce faux backend ;
//! 3. exécute le VRAI binaire `npu` (`env!("CARGO_BIN_EXE_npu")`, pas une
//!    fonction appelée directement dans ce processus de test) avec ce scope comme répertoire courant, et
//!    vérifie le code de sortie, stdout et stderr.
//!
//! `HOME` est redirigé vers le scope temporaire lui-même (qui ne contient
//! jamais `.config/npu`) et `XDG_CONFIG_HOME` est retiré de l'environnement
//! de l'enfant : seule la racine de scope temporaire (`<scope>/.npu`, via le
//! répertoire courant) doit être prise en compte par `scope::roots()`, jamais
//! le vrai `$HOME` ni un `/etc/npu` qui existerait par ailleurs sur la
//! machine. `Command::env`/`env_remove` ne touchent que l'environnement du
//! PROCESSUS ENFANT : aucun test ne mute les vraies variables d'environnement
//! (`std::env::set_var` est `unsafe` en édition 2024, interdit par
//! `unsafe_code = "forbid"`, cf. Cargo.toml).
//!
//! Aucun test n'appelle un vrai backend réseau : le seul réseau touché est le
//! listener bouchonné, local, créé par le test lui-même.

#![allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Crée un dossier de scope temporaire unique sous `target/`, distinct de la
/// fixture versionnée `.npu/` — même idiome que les fixtures de
/// `command::tests`/`config::tests`/`tests/cli.rs`.
fn fixture_scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-output-contract-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("création du dossier de scope temporaire");
    dir
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("création du dossier parent");
    }
    std::fs::write(path, contents).expect("écriture de la fixture");
}

/// Écrit un scope `.npu` complet (backend + modèle) pointant sur le faux
/// backend HTTP `addr`, plus la commande de fixture `e2e-cmd` dont la section
/// `[output]` est `output_section` (son contenu TOML, SANS les crochets
/// `[output]` eux-mêmes — chaque appelant de ce module en fournit un non
/// vide, cf. les quatre scénarios ci-dessous).
///
/// `scope` est le répertoire courant que reçoit le binaire (`run_npu`), PAS
/// la racine de scope elle-même : `scope::roots()` (`src/scope.rs`) calcule
/// la racine locale comme `<cwd>/.npu`, jamais `<cwd>` directement — chaque
/// chemin écrit ici doit donc être préfixé par `.npu/`, sous peine que le
/// binaire ne trouve ni backend, ni modèle, ni commande (bogue exact
/// reproduit et corrigé pendant l'écriture de ce test : une première version
/// écrivait directement sous `<scope>/backends/...`, que `scope::roots()`
/// ignore).
fn write_scope(scope: &Path, addr: std::net::SocketAddr, output_section: &str) {
    write(
        scope,
        ".npu/backends/stub.toml",
        &format!(
            r#"
            id = "stub"
            base_url = "http://{addr}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#
        ),
    );
    write(
        scope,
        ".npu/models/test-model.toml",
        r#"
        id = "test-model"
        backend = "stub"
        operation = "chat"
        model = "test-model"
        "#,
    );
    write(
        scope,
        ".npu/commands/e2e-cmd.md",
        &format!(
            "+++\nmodel = \"test-model\"\n\n[output]\n{output_section}\n+++\n{{{{ input }}}}\n"
        ),
    );
}

/// Délai maximal accordé au serveur bouchonné pour recevoir la connexion du
/// binaire `npu` lancé par `run_npu`. Borne le `accept()` (cf. plus bas)
/// plutôt que de le laisser bloquer indéfiniment : sans cette borne, un
/// changement futur qui ferait échouer `npu` AVANT qu'il ne contacte le
/// backend (une régression de validation de config, par exemple) ne
/// produirait pas un test rouge mais un `cargo test` qui ne se termine
/// jamais — un défaut constaté empiriquement pendant l'écriture de ce
/// fichier (bogue de résolution de scope ci-dessus, diagnostiqué via
/// `/proc/<pid>/task/*/wchan` après un blocage de plusieurs minutes).
const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Démarre un serveur HTTP bouchonné qui répond une seule fois avec le corps
/// `chat/completions` portant `content` comme message, puis se termine. Même
/// idiome que
/// `backend::tests::chat_end_to_end_against_stubbed_http_server`, généralisé
/// pour accepter un contenu de réponse arbitraire ET borner l'attente de
/// connexion (`ACCEPT_TIMEOUT`, cf. sa doc) plutôt que bloquer indéfiniment.
fn spawn_stub_server(content: String) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Read};
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind du listener bouchonné");
    let addr = listener.local_addr().expect("adresse locale du listener");
    listener
        .set_nonblocking(true)
        .expect("passage du listener en non-bloquant");

    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + ACCEPT_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "aucune connexion reçue sur le listener bouchonné dans le délai de \
                         {ACCEPT_TIMEOUT:?} : le binaire npu n'a jamais contacté le backend \
                         (a-t-il échoué plus tôt dans le pipeline ?)"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                // `panic!` direct : une erreur d'acceptation autre qu'un simple `WouldBlock`
                // (ex. le listener a été fermé) est un défaut du test lui-même, jamais un cas
                // à faire remonter proprement — même tolérance que pour un `#[test]` (cf.
                // Cargo.toml `[lints.clippy]`), ici nécessairement locale (`#[allow]` ci-dessous)
                // puisque ce code tourne dans le thread serveur, pas dans la fonction `#[test]`
                // elle-même.
                #[allow(clippy::panic)]
                Err(err) => panic!("acceptation de la connexion : {err}"),
            }
        };
        // Le flux accepté peut hériter le mode non bloquant du listener
        // selon la plateforme : repasser explicitement en bloquant, sinon la
        // lecture des en-têtes ci-dessous échouerait immédiatement en
        // `WouldBlock` plutôt que d'attendre la requête.
        stream
            .set_nonblocking(false)
            .expect("repassage du flux accepté en bloquant");
        let mut reader = BufReader::new(stream.try_clone().expect("clone du flux TCP"));

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

        let response_body = serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": content } }
            ]
        })
        .to_string();
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

    (addr, handle)
}

/// Exécute le VRAI binaire `npu` (compilé par cargo pour ce run de tests,
/// jamais une fonction appelée directement dans ce processus de test) avec
/// `scope` comme répertoire courant et `stdin_data` envoyé sur son entrée
/// standard, puis attend sa fin et renvoie sa sortie complète (code, stdout,
/// stderr).
fn run_npu(scope: &Path, args: &[&str], stdin_data: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(scope)
        // Isolation des scopes de configuration (npu-cli-spec.md §5) : seule
        // <scope>/.npu (via le répertoire courant) doit être vue. `HOME` est
        // redirigé vers `scope` lui-même (qui ne contient jamais
        // `.config/npu`) et `XDG_CONFIG_HOME` est retiré, pour que ni le vrai
        // $HOME ni un XDG_CONFIG_HOME hérité de l'environnement du test
        // n'introduisent une racine de scope parasite. Ceci ne touche que
        // l'environnement du PROCESSUS ENFANT, jamais les vraies variables
        // d'environnement de ce processus de test (`std::env::set_var` est
        // `unsafe`, interdit ici).
        .env("HOME", scope)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("lancement du binaire npu");

    // Fermer stdin (drop) après écriture : le mode d'entrée `stdin` de la
    // commande de fixture lit jusqu'à EOF (`read_to_string`), qui ne survient
    // jamais tant que le descripteur reste ouvert côté parent.
    {
        let stdin = child.stdin.as_mut().expect("stdin du processus enfant");
        stdin
            .write_all(stdin_data.as_bytes())
            .expect("écriture sur stdin de npu");
    }
    drop(child.stdin.take());

    child
        .wait_with_output()
        .expect("attente de la fin du processus npu")
}

/// Écrit un scope GÉNÉRAL (via `$XDG_CONFIG_HOME/npu`, jamais `<cwd>/.npu`)
/// contenant une commande `never-invoked` dont `[output].schema` pointe sur
/// `schemas/broken-or-missing.json` — cf. `src/scope.rs::candidate_roots` :
/// `$XDG_CONFIG_HOME/npu` est une racine de scope à part entière, plus
/// générale que `<cwd>/.npu`, exactement le niveau visé par la revue L3,
/// correctif 1 (« schéma cassé appartenant à une commande que personne
/// n'invoque »).
///
/// Si `schema_body` est `Some`, le fichier de schéma est écrit avec ce
/// contenu (utilisé pour simuler un JSON syntaxiquement cassé) ; si `None`,
/// il n'est jamais écrit du tout (schéma absent).
fn write_general_scope_with_never_invoked_command(
    xdg_root: &Path,
    base_url: &str,
    schema_body: Option<&str>,
) {
    write(
        xdg_root,
        "npu/backends/stub.toml",
        &format!(
            r#"
            id = "stub"
            base_url = "{base_url}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#
        ),
    );
    write(
        xdg_root,
        "npu/models/test-model.toml",
        r#"
        id = "test-model"
        backend = "stub"
        operation = "chat"
        model = "test-model"
        "#,
    );
    write(
        xdg_root,
        "npu/commands/never-invoked.md",
        "+++\nmodel = \"test-model\"\n\n[output]\nformat = \"json\"\n\
         schema = \"schemas/broken-or-missing.json\"\n+++\n{{ input }}\n",
    );
    if let Some(body) = schema_body {
        write(xdg_root, "npu/schemas/broken-or-missing.json", body);
    }
}

/// Exécute le VRAI binaire `npu` avec `$XDG_CONFIG_HOME` pointé sur
/// `xdg_config_home` (le scope GÉNÉRAL écrit par
/// `write_general_scope_with_never_invoked_command`) et `cwd` comme
/// répertoire courant — un répertoire délibérément SANS `.npu` local, pour
/// que la seule racine de scope prise en compte soit `$XDG_CONFIG_HOME/npu`
/// (cf. `src/scope.rs::candidate_roots`). `HOME` est redirigé vers `cwd`
/// (qui ne contient jamais `.config/npu`) pour la même raison d'isolation
/// que `run_npu`.
fn run_npu_xdg(cwd: &Path, xdg_config_home: &Path, args: &[&str], stdin_data: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("lancement du binaire npu");

    {
        let stdin = child.stdin.as_mut().expect("stdin du processus enfant");
        stdin
            .write_all(stdin_data.as_bytes())
            .expect("écriture sur stdin de npu");
    }
    drop(child.stdin.take());

    child
        .wait_with_output()
        .expect("attente de la fin du processus npu")
}

/// Crée un répertoire de travail temporaire sans `.npu` local, distinct du
/// scope général `$XDG_CONFIG_HOME/npu` — même idiome que `fixture_scope`.
fn fixture_cwd_without_local_scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-lazy-schema-cwd-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("création du répertoire de travail temporaire");
    dir
}

/// Revue L3, correctif 1, preuve (a) : un schéma ABSENT, déclaré par une
/// commande d'un scope général que personne n'invoque, ne doit plus
/// désactiver `npu --help` pour tout le CLI (résolution paresseuse de
/// l'existence, alignée sur la compilation paresseuse — cf.
/// `command::resolve_schema_path`).
#[test]
fn help_survives_a_missing_schema_declared_by_an_uninvoked_command_in_the_general_scope() {
    // L'URL de backend n'est ici qu'un détail d'infrastructure requis par
    // `write_general_scope_with_never_invoked_command` (le modèle a besoin
    // d'un backend valide pour que `config::load_scopes` réussisse) : elle
    // n'est JAMAIS contactée, `--help` ne contacte jamais aucun backend —
    // pas de serveur bouchonné à monter ici, contrairement à (c)/(d).
    let xdg = fixture_scope("xdg-missing-schema");
    let cwd = fixture_cwd_without_local_scope("missing-schema");
    // Schéma jamais écrit : `schemas/broken-or-missing.json` est absent du
    // disque.
    write_general_scope_with_never_invoked_command(&xdg, "http://127.0.0.1:1", None);

    let output = run_npu_xdg(&cwd, &xdg, &["--help"], "");

    assert!(
        output.status.success(),
        "PREUVE (a) : npu --help doit réussir (exit 0) même avec un schéma absent dans un \
         scope général, obtenu code {:?} ; stderr : {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Revue L3, correctif 1, preuve (b) : un schéma PRÉSENT mais syntaxiquement
/// CASSÉ, déclaré par une commande d'un scope général que personne
/// n'invoque, ne doit pas non plus désactiver `npu --help` — la compilation
/// du schéma reste paresseuse (règle 4 du contrat partagé), inchangée par ce
/// correctif.
#[test]
fn help_survives_a_syntactically_broken_schema_declared_by_an_uninvoked_command_in_the_general_scope()
 {
    let xdg = fixture_scope("xdg-broken-schema");
    let cwd = fixture_cwd_without_local_scope("broken-schema");
    write_general_scope_with_never_invoked_command(
        &xdg,
        "http://127.0.0.1:1",
        Some("{ ceci n'est pas du JSON"),
    );

    let output = run_npu_xdg(&cwd, &xdg, &["--help"], "");

    assert!(
        output.status.success(),
        "PREUVE (b) : npu --help doit réussir (exit 0) même avec un schéma syntaxiquement cassé \
         dans un scope général, obtenu code {:?} ; stderr : {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Revue L3, correctif 1, preuve (c) : invoquer RÉELLEMENT la commande dont
/// le schéma est absent doit échouer avec `Error::Config` (exit 2), nommant
/// à la fois le chemin résolu du schéma ET le fichier de commande qui le
/// réclame.
#[test]
fn invoking_the_command_with_a_missing_schema_fails_with_exit_code_two_naming_both_paths() {
    let (addr, server) = spawn_stub_server("{\"a\": 1}".to_string());

    let xdg = fixture_scope("xdg-missing-schema-invoked");
    let cwd = fixture_cwd_without_local_scope("missing-schema-invoked");
    write_general_scope_with_never_invoked_command(&xdg, &format!("http://{addr}"), None);

    let output = run_npu_xdg(&cwd, &xdg, &["never-invoked"], "peu importe");
    server
        .join()
        .expect("le thread serveur ne doit pas paniquer");

    assert_eq!(
        output.status.code(),
        Some(2),
        "PREUVE (c) : un schéma absent découvert À L'USAGE doit échouer avec le code 2 \
         (Error::Config — la configuration est cassée, pas la réponse du modèle) ; stderr : {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "rien ne doit être écrit sur stdout en cas d'échec du contrat de sortie, obtenu : {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr doit être de l'UTF-8 valide");
    assert!(
        stderr.contains("broken-or-missing.json"),
        "PREUVE (c) : stderr doit nommer le chemin résolu du schéma, obtenu : {stderr}"
    );
    assert!(
        stderr.contains("never-invoked.md"),
        "PREUVE (c) : stderr doit aussi nommer le fichier de commande fautif, obtenu : {stderr}"
    );
}

/// Revue L3, correctif 1, preuve (d) : même exigence que (c), pour un schéma
/// PRÉSENT mais syntaxiquement CASSÉ.
#[test]
fn invoking_the_command_with_a_broken_schema_fails_with_exit_code_two_naming_both_paths() {
    let (addr, server) = spawn_stub_server("{\"a\": 1}".to_string());

    let xdg = fixture_scope("xdg-broken-schema-invoked");
    let cwd = fixture_cwd_without_local_scope("broken-schema-invoked");
    write_general_scope_with_never_invoked_command(
        &xdg,
        &format!("http://{addr}"),
        Some("{ ceci n'est pas du JSON"),
    );

    let output = run_npu_xdg(&cwd, &xdg, &["never-invoked"], "peu importe");
    server
        .join()
        .expect("le thread serveur ne doit pas paniquer");

    assert_eq!(
        output.status.code(),
        Some(2),
        "PREUVE (d) : un schéma cassé découvert À L'USAGE doit échouer avec le code 2 \
         (Error::Config) ; stderr : {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "rien ne doit être écrit sur stdout en cas d'échec du contrat de sortie, obtenu : {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr doit être de l'UTF-8 valide");
    assert!(
        stderr.contains("broken-or-missing.json"),
        "PREUVE (d) : stderr doit nommer le chemin résolu du schéma, obtenu : {stderr}"
    );
    assert!(
        stderr.contains("never-invoked.md"),
        "PREUVE (d) : stderr doit aussi nommer le fichier de commande fautif, obtenu : {stderr}"
    );
}

/// a) le modèle répond du JSON emballé dans une clôture Markdown -> stdout
/// reçoit du JSON compact valide, exit 0.
#[test]
fn json_wrapped_in_fence_and_schema_satisfied_succeeds_end_to_end() {
    let (addr, server) =
        spawn_stub_server("```json\n{\"category\": \"bug\", \"confidence\": 0.9}\n```".to_string());

    let scope = fixture_scope("fenced-json-ok");
    let schema = r#"{
        "type": "object",
        "required": ["category", "confidence"],
        "properties": {
            "category": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "additionalProperties": false
    }"#;
    write(&scope, ".npu/schemas/classification.json", schema);
    write_scope(
        &scope,
        addr,
        "format = \"json\"\nschema = \"schemas/classification.json\"",
    );

    let output = run_npu(&scope, &["e2e-cmd"], "peu importe");
    server
        .join()
        .expect("le thread serveur ne doit pas paniquer");

    assert!(
        output.status.success(),
        "code de sortie attendu 0, obtenu {:?} ; stderr : {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout doit être de l'UTF-8 valide");
    assert_eq!(
        stdout, "{\"category\":\"bug\",\"confidence\":0.9}\n",
        "stdout doit contenir EXACTEMENT la sérialisation JSON COMPACTE suivie d'un unique \
         retour à la ligne (ajouté par `run()`, §14/§22 : npu classify | jq .), quel que soit \
         l'emballage Markdown renvoyé par le modèle — rien avant, rien après"
    );
}

/// b) le modèle répond du JSON qui viole le schéma -> exit 4, message sur
/// stderr, rien sur stdout.
#[test]
fn json_violating_schema_fails_with_exit_code_four_end_to_end() {
    // « confidence » manquant : viole `required`.
    let (addr, server) = spawn_stub_server("{\"category\": \"bug\"}".to_string());

    let scope = fixture_scope("schema-violation");
    let schema = r#"{
        "type": "object",
        "required": ["category", "confidence"],
        "properties": {
            "category": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "additionalProperties": false
    }"#;
    write(&scope, ".npu/schemas/classification.json", schema);
    write_scope(
        &scope,
        addr,
        "format = \"json\"\nschema = \"schemas/classification.json\"",
    );

    let output = run_npu(&scope, &["e2e-cmd"], "peu importe");
    server
        .join()
        .expect("le thread serveur ne doit pas paniquer");

    assert_eq!(
        output.status.code(),
        Some(4),
        "une sortie JSON qui viole le schéma déclaré doit échouer avec le code 4 \
         (Error::Output — la config est valide, c'est le modèle qui a mal répondu) ; \
         stderr : {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "rien ne doit être écrit sur stdout en cas d'échec du contrat de sortie, obtenu : {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr doit être de l'UTF-8 valide");
    assert!(
        stderr.contains("confidence"),
        "stderr doit nommer la violation (propriété requise manquante), obtenu : {stderr}"
    );
}

/// c) le modèle répond du texte qui n'est pas du JSON alors que
/// format = "json" -> exit 4.
#[test]
fn non_json_response_with_json_format_fails_with_exit_code_four_end_to_end() {
    let (addr, server) = spawn_stub_server("ceci n'est pas du JSON du tout".to_string());

    let scope = fixture_scope("non-json-response");
    // Pas de schéma déclaré : règle 2 du contrat partagé — format = "json"
    // sans schema est autorisé, on valide alors seulement que la sortie est
    // du JSON bien formé.
    write_scope(&scope, addr, "format = \"json\"");

    let output = run_npu(&scope, &["e2e-cmd"], "peu importe");
    server
        .join()
        .expect("le thread serveur ne doit pas paniquer");

    assert_eq!(
        output.status.code(),
        Some(4),
        "une réponse qui n'est pas du JSON alors que format = \"json\" doit échouer avec le \
         code 4 ; stderr : {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

/// d) une commande format = "text" avec `max_lines` = 1 dont le modèle renvoie
/// trois lignes -> exit 4.
#[test]
fn text_exceeding_max_lines_fails_with_exit_code_four_end_to_end() {
    let (addr, server) = spawn_stub_server("ligne 1\nligne 2\nligne 3".to_string());

    let scope = fixture_scope("max-lines-exceeded");
    write_scope(&scope, addr, "format = \"text\"\nmax_lines = 1");

    let output = run_npu(&scope, &["e2e-cmd"], "peu importe");
    server
        .join()
        .expect("le thread serveur ne doit pas paniquer");

    assert_eq!(
        output.status.code(),
        Some(4),
        "une réponse texte dépassant max_lines doit échouer avec le code 4, jamais être \
         tronquée silencieusement (§15) ; stderr : {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr doit être de l'UTF-8 valide");
    assert!(
        stderr.contains('1') && stderr.contains('3'),
        "stderr doit citer le nombre attendu et le nombre reçu de lignes, obtenu : {stderr}"
    );
}
