//! Token handling. `gh` is faked with shell scripts; the real Keychain is
//! only touched with `HERSE_SHARE_KEYCHAIN_TESTS=1` (under a test-only
//! service name).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use herse_share::{Keychain, ShareError, Token, TokenKind, find_gh_in, gh_token_with};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "herse-share-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn script(&self, sub: &str, name: &str, body: &str, executable: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.0.join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn tokens_are_validated_and_redacted() {
    let t = Token::new("  gho_abc123XYZ\n").unwrap();
    assert_eq!(t.expose(), "gho_abc123XYZ");
    assert_eq!(t.kind(), TokenKind::OAuth);
    assert_eq!(format!("{t:?}"), "Token(OAuth, <redacted>)");
    assert_eq!(Token::new("ghp_x").unwrap().kind(), TokenKind::Classic);
    assert_eq!(
        Token::new("github_pat_11AB_cd").unwrap().kind(),
        TokenKind::FineGrained
    );
    assert_eq!(Token::new("ghs_x").unwrap().kind(), TokenKind::App);
    assert_eq!(
        Token::new("0123456789abcdef0123456789abcdef01234567")
            .unwrap()
            .kind(),
        TokenKind::Other
    );
    for bad in [
        "",
        "   ",
        "Bearer ghp_x",
        "ghp_x\nX-Injected: 1",
        "ghp_x;",
        "tök",
    ] {
        assert!(
            matches!(Token::new(bad), Err(ShareError::InvalidInput(_))),
            "{bad:?}"
        );
    }
    assert!(Token::new("a".repeat(256)).is_err());
}

#[test]
fn finds_gh_on_path_then_fallback_dirs() {
    let tmp = TempDir::new();
    tmp.script("empty", "other-tool", "true", true);
    tmp.script("not-exec", "gh", "true", false);
    let fallback = tmp.script("homebrew", "gh", "true", true);
    let path_var =
        std::env::join_paths([tmp.0.join("empty"), tmp.0.join("not-exec"), PathBuf::new()])
            .unwrap();
    let extra = [tmp.0.join("missing"), tmp.0.join("homebrew")];
    let extra: Vec<&Path> = extra.iter().map(PathBuf::as_path).collect();
    assert_eq!(find_gh_in(Some(&path_var), &extra), Some(fallback.clone()));
    assert_eq!(find_gh_in(None, &extra), Some(fallback));

    // PATH wins over the fallbacks.
    let on_path = tmp.script("bin", "gh", "true", true);
    let path_var = std::env::join_paths([tmp.0.join("bin")]).unwrap();
    assert_eq!(find_gh_in(Some(&path_var), &extra), Some(on_path));
    assert_eq!(find_gh_in(None, &[]), None);
}

#[test]
fn gh_token_from_a_fake_cli() {
    let tmp = TempDir::new();
    let signed_in = tmp.script(
        "a",
        "gh",
        r#"if [ "$*" = "auth token --hostname github.com" ] && [ "$GH_PROMPT_DISABLED" = "1" ]; then
  echo "gho_FakeToken0123456789"; exit 0
fi
echo "unexpected: $*" >&2; exit 2"#,
        true,
    );
    let token = gh_token_with(&signed_in, Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(token.expose(), "gho_FakeToken0123456789");

    let signed_out = tmp.script(
        "b",
        "gh",
        "echo 'no oauth token found for github.com' >&2; exit 1",
        true,
    );
    assert_eq!(
        gh_token_with(&signed_out, Duration::from_secs(10)),
        Ok(None)
    );

    let old_gh = tmp.script(
        "c",
        "gh",
        "echo 'unknown command \"token\" for \"gh auth\"' >&2; exit 1",
        true,
    );
    assert_eq!(gh_token_with(&old_gh, Duration::from_secs(10)), Ok(None));

    let garbage = tmp.script("d", "gh", "echo 'Welcome to GitHub CLI!'", true);
    assert!(matches!(
        gh_token_with(&garbage, Duration::from_secs(10)),
        Err(ShareError::GhCli(_))
    ));

    let hangs = tmp.script("e", "gh", "exec sleep 30", true);
    let start = Instant::now();
    assert!(matches!(
        gh_token_with(&hangs, Duration::from_millis(300)),
        Err(ShareError::GhCli(_))
    ));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "killed on timeout"
    );

    let missing = tmp.0.join("nope/gh");
    assert!(matches!(
        gh_token_with(&missing, Duration::from_secs(1)),
        Err(ShareError::GhCli(_))
    ));
}

#[test]
fn keychain_rejects_bad_input_before_running_security() {
    // Validation happens before any `security` call, so this touches nothing.
    let token = Token::new("ghp_x").unwrap();
    for service in ["", "Herse \"GitHub\"", "back\\slash", "new\nline"] {
        let kc = Keychain::new(service);
        assert!(
            matches!(kc.load(), Err(ShareError::InvalidInput(_))),
            "{service:?}"
        );
        assert!(
            matches!(
                kc.store("octocat", &token),
                Err(ShareError::InvalidInput(_))
            ),
            "{service:?}"
        );
        assert!(
            matches!(kc.delete(), Err(ShareError::InvalidInput(_))),
            "{service:?}"
        );
    }
    let kc = Keychain::new("Herse GitHub (herse-share test)");
    for login in ["", "has space", "quote\"", "a".repeat(40).as_str()] {
        assert!(
            matches!(kc.store(login, &token), Err(ShareError::InvalidInput(_))),
            "{login:?}"
        );
    }
    assert_eq!(Keychain::default().service(), "Herse GitHub");
}

/// Opt-in: `HERSE_SHARE_KEYCHAIN_TESTS=1 cargo test -p herse-share keychain`.
/// Uses its own service name and deletes the item afterwards.
#[test]
fn keychain_round_trip() {
    if std::env::var("HERSE_SHARE_KEYCHAIN_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set HERSE_SHARE_KEYCHAIN_TESTS=1 to test against the real Keychain");
        return;
    }
    let kc = Keychain::new("Herse GitHub (herse-share test)");
    kc.delete().unwrap();
    assert_eq!(kc.load().unwrap(), None);
    let first = Token::new("ghp_HerseKeychainTest0001").unwrap();
    kc.store("octocat", &first).unwrap();
    assert_eq!(kc.load().unwrap(), Some(first));
    // Storing again (another account) replaces it.
    let second = Token::new("github_pat_HerseKeychainTest_0002").unwrap();
    kc.store("hubot", &second).unwrap();
    assert_eq!(kc.load().unwrap(), Some(second));
    assert!(kc.delete().unwrap());
    assert!(!kc.delete().unwrap());
    assert_eq!(kc.load().unwrap(), None);
}
