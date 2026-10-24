//! The pre-publish scanner: true positives, no false positives (including on
//! the app's fixture corpus), redaction, places, sizes.

use std::path::{Path, PathBuf};
use std::time::Instant;

use herse_nb::{Cell, CellType, MimeBundle, MimeData, Notebook, Output, StreamName};
use herse_share::{Place, ScanOptions, SecretKind, WarningKind, scan};
use serde_json::json;

const HOME: &str = "/Users/herse-test";

fn opts() -> ScanOptions {
    ScanOptions {
        home_dir: Some(HOME.into()),
        ..ScanOptions::default()
    }
}

fn notebook(sources: &[&str]) -> Notebook {
    let mut nb = Notebook::new_empty("python3", "Python 3", "python");
    nb.cells.clear();
    for s in sources {
        nb.cells
            .push(Cell::with_new_id(CellType::Code).with_source(*s));
    }
    nb
}

/// Fake secrets, assembled at runtime so this file holds no token-shaped literals.
struct Fakes {
    aws_id: String,
    aws_secret: String,
    github: String,
    github_pat: String,
    openai: String,
    openai_classic: String,
    anthropic: String,
    google: String,
    slack: String,
    stripe: String,
    hf: String,
    jwt: String,
    private_key: String,
}

fn fakes() -> Fakes {
    Fakes {
        aws_id: format!("{}{}", "AK", "IAQ3EXAMPLE7ZKXV2P"),
        aws_secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCY".to_string() + "EXAMPLEKEY",
        github: format!("ghp{}{}", "_", "a1B2".repeat(9)),
        github_pat: format!(
            "github{}pat_{}_{}",
            "_",
            "11AAAAAAA0".repeat(2) + "Zz",
            "x9".repeat(30)
        ),
        openai: format!("sk-{}-{}", "proj", "Ab3_dE-f".repeat(6)),
        openai_classic: format!("sk-{}", "Ab3dEf9GhJ".repeat(4)),
        anthropic: format!("sk-{}-api03-{}", "ant", "Zx9-Yw8_".repeat(12)),
        google: format!("AI{}{}", "za", "SyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q"),
        slack: format!("xo{}-{}", "xb", "1234567890-abcdefghij"),
        stripe: format!("sk_{}_{}", "live", "Zz9".repeat(10)),
        hf: format!("hf_{}", "aBcD1234".repeat(5)),
        jwt: [
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ]
        .join("."),
        private_key: format!(
            "-----BEGIN {} PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----",
            "RSA"
        ),
    }
}

fn secret_kinds(nb: &Notebook) -> Vec<SecretKind> {
    let mut kinds: Vec<SecretKind> = scan(nb, &opts())
        .warnings
        .iter()
        .filter_map(|w| match w.kind {
            WarningKind::Secret(k) => Some(k),
            _ => None,
        })
        .collect();
    kinds.sort();
    kinds
}

#[test]
fn finds_each_kind_of_secret() {
    let f = fakes();
    let cases: Vec<(String, SecretKind)> = vec![
        (
            format!("aws_access_key_id = '{}'", f.aws_id),
            SecretKind::AwsAccessKeyId,
        ),
        (
            format!("aws_secret_access_key = \"{}\"", f.aws_secret),
            SecretKind::AwsSecretKey,
        ),
        (
            format!("!git clone https://{}@github.com/me/repo", f.github),
            SecretKind::GitHubToken,
        ),
        (
            format!("gh = Github(\"{}\")", f.github_pat),
            SecretKind::GitHubToken,
        ),
        (
            format!("client = OpenAI(api_key=\"{}\")", f.openai),
            SecretKind::OpenAiKey,
        ),
        (
            format!("openai.api_key = '{}'", f.openai_classic),
            SecretKind::OpenAiKey,
        ),
        (
            format!("client = anthropic.Anthropic(api_key=\"{}\")", f.anthropic),
            SecretKind::AnthropicKey,
        ),
        (
            format!("genai.configure(api_key=\"{}\")", f.google),
            SecretKind::GoogleApiKey,
        ),
        (
            format!("slack = WebClient(token=\"{}\")", f.slack),
            SecretKind::SlackToken,
        ),
        (
            format!("stripe.api_key = \"{}\"", f.stripe),
            SecretKind::StripeKey,
        ),
        (
            format!("login(token=\"{}\")", f.hf),
            SecretKind::HuggingFaceToken,
        ),
        (
            format!("headers = {{'Authorization': 'Bearer {}'}}", f.jwt),
            SecretKind::Jwt,
        ),
        (
            format!("key = \"\"\"{}\"\"\"", f.private_key),
            SecretKind::PrivateKey,
        ),
        (
            "conn = psycopg2.connect(host='db', user='me', password='S3cr3t!x')".into(),
            SecretKind::Password,
        ),
        ("DB_PASSWORD = \"hunter22\"".into(), SecretKind::Password),
        (
            "engine = create_engine('postgresql://analyst:Tr0ub4dor3@db.internal:5432/sales')"
                .into(),
            SecretKind::UrlCredentials,
        ),
        (
            "%env WEATHER_API_KEY=a8f5f167f44f4964e6c9".into(),
            SecretKind::SecretAssignment,
        ),
        (
            "export SERVICE_TOKEN=z8x7c6v5b4n3m2a1q0".into(),
            SecretKind::SecretAssignment,
        ),
        (
            "os.environ[\"MAPBOX_ACCESS_TOKEN\"] = \"pk9q8w7e6r5t4y3u2i1o\"".into(),
            SecretKind::SecretAssignment,
        ),
        (
            "config = {\"client_secret\": \"9f8e7d6c5b4a39281706\"}".into(),
            SecretKind::SecretAssignment,
        ),
        (
            "apiKey: 'Q2w3E4r5T6y7U8i9O0p1'".into(),
            SecretKind::SecretAssignment,
        ),
    ];
    for (source, kind) in &cases {
        let nb = notebook(&[source]);
        assert_eq!(secret_kinds(&nb), vec![*kind], "{source}");
    }
}

#[test]
fn excerpts_are_redacted_and_located() {
    let f = fakes();
    let source = format!(
        "import os\n\nclient = OpenAI(api_key=\"{}\")\nprint(1)\nretry = \"{}\"\n",
        f.openai, f.openai
    );
    let mut nb = notebook(&["print('setup')", &source]);
    nb.cells[1].id = Some("cell-two".into());
    let report = scan(&nb, &opts());
    assert_eq!(report.warnings.len(), 1, "{:#?}", report.warnings);
    let w = &report.warnings[0];
    assert_eq!(w.kind, WarningKind::Secret(SecretKind::OpenAiKey));
    assert_eq!(w.location.cell, Some(1));
    assert_eq!(w.location.cell_id.as_deref(), Some("cell-two"));
    assert_eq!(w.location.place, Place::Source);
    assert_eq!(w.location.line, Some(3));
    assert_eq!(w.count, 2);
    assert_eq!(w.excerpt, "sk-…");
    assert_eq!(
        w.message(),
        "OpenAI API key (sk-…) in cell 2 source, line 3 (2 times)"
    );
    assert!(report.has_secrets());

    // No excerpt or message ever contains a secret.
    let all = [
        f.aws_id.clone(),
        f.aws_secret.clone(),
        f.github.clone(),
        f.github_pat.clone(),
        f.openai.clone(),
        f.anthropic.clone(),
        f.google.clone(),
        f.slack.clone(),
        f.stripe.clone(),
        f.hf.clone(),
        f.jwt.clone(),
        "hunter22".into(),
        "Tr0ub4dor3".into(),
        "9f8e7d6c5b4a39281706".into(),
    ];
    let source = format!(
        "a = '{}'\naws_secret_access_key='{}'\nb = '{}'\nc = '{}'\nd = '{}'\ne = '{}'\nf = '{}'\ng = '{}'\nh = '{}'\ni = '{}'\nj = '{}'\npassword = 'hunter22'\nu = 'mysql://root:Tr0ub4dor3@localhost/db'\nclient_secret = '9f8e7d6c5b4a39281706'\n",
        f.aws_id,
        f.aws_secret,
        f.github,
        f.github_pat,
        f.openai,
        f.anthropic,
        f.google,
        f.slack,
        f.stripe,
        f.hf,
        f.jwt
    );
    let report = scan(&notebook(&[&source]), &opts());
    assert!(report.warnings.len() >= 12, "{:#?}", report.warnings);
    for w in &report.warnings {
        for secret in &all {
            assert!(
                !w.excerpt.contains(secret.as_str()) && !w.message().contains(secret.as_str()),
                "{w:?}"
            );
        }
    }
}

#[test]
fn no_false_positives_on_ordinary_code() {
    let benign = [
        "from transformers import AutoTokenizer\ntokenizer = AutoTokenizer.from_pretrained(\"bert-base-uncased\")",
        "max_tokens = 4096\nMAX_TOKENS=4096\nTOKEN_LIMIT=100000000\ntoken = \"hello\"\ntoken_type = \"Bearer\"",
        "password = getpass.getpass(\"Password: \")\npassword = os.environ[\"DB_PASSWORD\"]",
        "api_key = os.getenv(\"OPENAI_API_KEY\")\nAPI_KEY = \"<your-api-key>\"\nOPENAI_API_KEY=\"sk-...\"",
        "secret = \"${SECRET}\"\nos.environ[\"OPENAI_API_KEY\"] = \"YOUR_KEY_HERE\"\npassword = \"********\"",
        "url = \"https://user@github.com/x/y.git\"\nconn = \"postgresql://user:${PASSWORD}@localhost/db\"",
        "dsn = 'postgres://user:password@localhost:5432/db'\nproxy = 'http://proxy.corp:8080/path@v2'",
        "import sklearn  # sk-learn\n# task-runner-configuration-for-the-cluster-0001",
        "credentials_path = \"/secrets/service-account.json\"\nCREDENTIALS_FILE = \"service-account-2024.json\"",
        "key = \"a1b2c3d4e5f6g7h8\"\ncache_key = \"user_1234567890abc\"\nhash = \"d41d8cd98f00b204e9800998ecf8427e\"",
        "password_hint = \"Your pet\"\nprint(f\"{password=}\")\nif password == \"\": pass",
        "df = pd.read_csv('data/2024-sales.csv')\nmonkey = 'banana1234567890'\nsecret_santa = 'Bob'",
        "%env PYTHONPATH=/opt/lib\nexport PATH=$PATH:/usr/local/bin\n!pip install keyring==24.3.1",
        "A_KEY_PLAYER = 'messi'\nPRIMARY_KEY = 'id'\nsort_key = 'created_at'",
        "var t = {tiles: [\"https://tiles.example/{z}/{x}/{y}.png?api_key=\"], tileSize: 256}; x = \"?api_key=\"===a[0].slice(-9)",
        "<img src=\"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==\">",
    ];
    for source in benign {
        let report = scan(&notebook(&[source]), &opts());
        assert!(
            report.warnings.is_empty(),
            "{source}\n{:#?}",
            report.warnings
        );
    }
}

#[test]
fn scans_outputs_and_metadata() {
    let f = fakes();
    let mut nb = notebook(&["run()"]);
    let mut html = MimeBundle::new();
    html.insert("text/plain", MimeData::Text("<Config>".into()));
    html.insert("text/html", MimeData::Text(format!("<td>{}</td>", f.slack)));
    let mut json_out = MimeBundle::new();
    json_out.insert(
        "application/json",
        MimeData::Json(json!({"auth": {"token": f.hf}})),
    );
    let mut image = MimeBundle::new();
    image.insert("image/png", MimeData::Text(f.github.clone())); // binary payloads are skipped
    nb.cells[0].outputs = vec![
        Output::Stream {
            name: StreamName::Stdout,
            text: format!("using key {}\n", f.aws_id),
        },
        Output::Error {
            ename: "ConnectionError".into(),
            evalue: "boom".into(),
            traceback: vec![
                format!("File \"{HOME}/projects/etl/load.py\", line 3"),
                format!("{HOME}/venv/lib/x.py"),
            ],
        },
        Output::DisplayData {
            data: html,
            metadata: Default::default(),
            transient: None,
        },
        Output::ExecuteResult {
            execution_count: Some(1),
            data: json_out,
            metadata: Default::default(),
        },
        Output::DisplayData {
            data: image,
            metadata: Default::default(),
            transient: None,
        },
    ];
    nb.cells[0].metadata.insert(
        "papermill".into(),
        json!({"parameters": {"db_password": "Pa55word!"}}),
    );
    nb.metadata.insert(
        "connection".into(),
        json!({"url": "redis://default:Sup3rS3cret@cache:6379"}),
    );

    let report = scan(&nb, &opts());
    let got: Vec<(Option<usize>, Place, WarningKind, usize)> = report
        .warnings
        .iter()
        .map(|w| (w.location.cell, w.location.place, w.kind, w.count))
        .collect();
    use SecretKind::*;
    assert_eq!(
        got,
        vec![
            (
                None,
                Place::NotebookMetadata,
                WarningKind::Secret(UrlCredentials),
                1
            ),
            (
                Some(0),
                Place::CellMetadata,
                WarningKind::Secret(Password),
                1
            ),
            (
                Some(0),
                Place::Output(0),
                WarningKind::Secret(AwsAccessKeyId),
                1
            ),
            (Some(0), Place::Output(1), WarningKind::HomePath, 2),
            (
                Some(0),
                Place::Output(2),
                WarningKind::Secret(SlackToken),
                1
            ),
            (
                Some(0),
                Place::Output(3),
                WarningKind::Secret(HuggingFaceToken),
                1
            ),
        ]
    );
    let home = report
        .warnings
        .iter()
        .find(|w| w.kind == WarningKind::HomePath)
        .unwrap();
    assert_eq!(home.excerpt, format!("{HOME}/projects/etl/load.py"));
    assert_eq!(home.location.line, None);
    assert!(home.removed_by_strip_outputs());

    // Without outputs (they'll be stripped), only source and metadata count.
    let report = scan(
        &nb,
        &ScanOptions {
            include_outputs: false,
            ..opts()
        },
    );
    assert_eq!(report.warnings.len(), 2);
    assert!(
        report
            .warnings
            .iter()
            .all(|w| !w.removed_by_strip_outputs())
    );
}

#[test]
fn home_paths() {
    let nb = notebook(&[
        &format!("df = pd.read_csv('{HOME}/Downloads/data.csv')\nopen(\"{HOME}\")"),
        "path = '/Users/herse-tester/data.csv'\nother = '/Users/someone/x'",
    ]);
    let report = scan(&nb, &opts());
    assert_eq!(report.warnings.len(), 1, "{:#?}", report.warnings);
    let w = &report.warnings[0];
    assert_eq!(
        (w.kind, w.location.cell, w.location.line, w.count),
        (WarningKind::HomePath, Some(0), Some(1), 2)
    );
    assert_eq!(w.excerpt, format!("{HOME}/Downloads/data.csv"));
    assert_eq!(
        w.message(),
        format!("Local path {HOME}/Downloads/data.csv in cell 1 source, line 1 (2 times)")
    );

    // A trailing slash in $HOME is fine; no home or "/" disables the check.
    let slash = ScanOptions {
        home_dir: Some(format!("{HOME}/")),
        ..opts()
    };
    assert_eq!(scan(&nb, &slash).warnings.len(), 1);
    for home in [None, Some("/".to_string()), Some(String::new())] {
        assert!(
            scan(
                &nb,
                &ScanOptions {
                    home_dir: home,
                    ..opts()
                }
            )
            .warnings
            .is_empty()
        );
    }
}

#[test]
fn sizes_and_widget_state() {
    let mut nb = notebook(&["plot()", "print(1)"]);
    let mut big = MimeBundle::new();
    big.insert("image/png", MimeData::Text("A".repeat(1_500_000)));
    nb.cells[0].outputs.push(Output::DisplayData {
        data: big,
        metadata: Default::default(),
        transient: None,
    });
    nb.cells[1].outputs.push(Output::Stream {
        name: StreamName::Stdout,
        text: "1\n".into(),
    });
    nb.metadata.insert(
        "widgets".into(),
        json!({"application/vnd.jupyter.widget-state+json": {"state": {"m1": {"model_name": "IntSliderModel"}}, "version_major": 2}}),
    );
    let report = scan(&nb, &opts());
    let kinds: Vec<_> = report
        .warnings
        .iter()
        .map(|w| (w.kind, w.location.cell, w.location.place))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (WarningKind::WidgetState, None, Place::NotebookMetadata),
            (WarningKind::LargeOutput, Some(0), Place::Output(0)),
        ]
    );
    assert_eq!(report.warnings[1].bytes, Some(1_500_000));
    assert_eq!(
        report.warnings[1].message(),
        "Large output (1.4 MB) in cell 1 output"
    );
    assert!(
        report.warnings[0]
            .message()
            .starts_with("Saved widget state (")
    );
    assert!(report.notebook_bytes > 1_500_000);
    assert!(!report.has_secrets());

    let small = ScanOptions {
        large_notebook_bytes: 1_000_000,
        ..opts()
    };
    let report = scan(&nb, &small);
    assert_eq!(report.warnings[0].kind, WarningKind::LargeNotebook);
    assert_eq!(report.warnings[0].location.place, Place::Notebook);

    // Capped, notebook-level warnings first.
    let many: Vec<String> = (0..50)
        .map(|i| format!("password = 'hunter{i:02}'"))
        .collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let mut nb = notebook(&refs);
    nb.metadata.insert("widgets".into(), json!({}));
    let report = scan(
        &nb,
        &ScanOptions {
            max_warnings: 10,
            ..opts()
        },
    );
    assert_eq!(report.warnings.len(), 10);
    assert_eq!(report.omitted, 41);
    assert_eq!(report.warnings[0].kind, WarningKind::WidgetState);
}

fn notebooks_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            notebooks_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "ipynb") {
            out.push(path);
        }
    }
}

/// The app's fixture corpus (real executed notebooks: pandas, matplotlib,
/// plotly/bokeh JS bundles, tracebacks, widgets) has no secrets.
#[test]
fn no_false_positives_on_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut paths = Vec::new();
    notebooks_under(&root, &mut paths);
    paths.sort();
    assert!(
        paths.len() >= 4,
        "fixtures not found under {}",
        root.display()
    );

    let mut scan_time = std::time::Duration::ZERO;
    let mut slowest = (std::time::Duration::ZERO, PathBuf::new());
    let (mut scanned, mut bytes, mut large_outputs) = (0, 0, 0);
    let mut false_positives = Vec::new();
    for path in &paths {
        let data = std::fs::read(path).unwrap();
        let Ok(nb) = herse_nb::parse(&data) else {
            continue;
        }; // corrupt_* fixtures
        scanned += 1;
        bytes += data.len();
        let start = Instant::now();
        let report = scan(&nb, &opts());
        let took = start.elapsed();
        scan_time += took;
        if took > slowest.0 {
            slowest = (took, path.clone());
        }
        for w in &report.warnings {
            match w.kind {
                WarningKind::Secret(_) | WarningKind::HomePath => {
                    false_positives.push(format!("{}: {}", path.display(), w.message()))
                }
                WarningKind::LargeOutput => large_outputs += 1,
                _ => {}
            }
        }
        if path.ends_with("fixtures/plots.ipynb") {
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.kind == WarningKind::LargeOutput),
                "plotly's 4.5 MB bundle"
            );
        }
    }
    eprintln!(
        "scanned {scanned} fixture notebooks ({:.1} MB) in {scan_time:?} (slowest {:?}: {}); {large_outputs} large outputs",
        bytes as f64 / 1e6,
        slowest.0,
        slowest.1.display()
    );
    assert!(false_positives.is_empty(), "{}", false_positives.join("\n"));
}
