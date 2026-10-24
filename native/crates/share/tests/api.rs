//! The gist client and publish flow against a local mock server.

mod common;

use std::net::TcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{FAKE_TOKEN, FakeComment, FakeGitHub, MockServer, Reply, sample_notebook};
use herse_share::*;
use serde_json::json;

fn client_for(server: &MockServer) -> GistClient {
    GistClient::with_base_url(Token::new(FAKE_TOKEN).unwrap(), &server.url())
}

#[test]
fn whoami_sends_github_headers_and_reads_scopes() {
    let server = MockServer::start(|_| {
        Reply::json(
            200,
            json!({"login": "octocat", "name": "The Octocat", "avatar_url": "https://a.example/1"}),
        )
        .header("X-OAuth-Scopes", "gist, read:org")
    });
    let me = client_for(&server).whoami().unwrap();
    assert_eq!(me.login, "octocat");
    assert_eq!(me.name.as_deref(), Some("The Octocat"));
    assert_eq!(
        me.scopes,
        Some(vec!["gist".to_string(), "read:org".to_string()])
    );
    assert_eq!(me.can_write_gists(), Some(true));

    let req = &server.requests()[0];
    assert_eq!((req.method.as_str(), req.path.as_str()), ("GET", "/user"));
    assert_eq!(
        req.header("authorization"),
        Some(format!("Bearer {FAKE_TOKEN}").as_str())
    );
    assert_eq!(req.header("accept"), Some("application/vnd.github+json"));
    assert_eq!(req.header("x-github-api-version"), Some("2022-11-28"));
    assert!(req.header("user-agent").unwrap().starts_with("Herse/"));

    // Fine-grained tokens report no scopes; classic tokens without `gist` can't write.
    let fine = MockServer::start(|_| Reply::json(200, json!({"login": "octocat"})));
    assert_eq!(client_for(&fine).whoami().unwrap().can_write_gists(), None);
    let classic = MockServer::start(|_| {
        Reply::json(200, json!({"login": "octocat"})).header("X-OAuth-Scopes", "repo")
    });
    assert_eq!(
        client_for(&classic).whoami().unwrap().can_write_gists(),
        Some(false)
    );
}

#[test]
fn gist_create_get_update_rename_delete() {
    let gh = FakeGitHub::start("octocat");
    let client = gh.client();
    let content = "{\"cells\": []}\n";
    let gist = client
        .create_gist(&NewGist {
            filename: "analysis.ipynb",
            content,
            description: "Analysis",
            public: false,
        })
        .unwrap();
    let body = gh.server.requests()[0].json();
    assert_eq!(
        body,
        json!({"description": "Analysis", "public": false, "files": {"analysis.ipynb": {"content": content}}})
    );
    assert_eq!(
        gist.html_url,
        format!("https://gist.github.com/{}", gist.id)
    );
    assert_eq!(
        gist.nbviewer_url(),
        format!("https://nbviewer.org/gist/{}", gist.id)
    );
    assert!(!gist.public);
    assert_eq!(gist.owner.as_deref(), Some("octocat"));
    assert_eq!(gist.description.as_deref(), Some("Analysis"));
    assert_eq!(gist.revisions, 1);
    assert_eq!(gist.files.len(), 1);
    assert_eq!(gist.files[0].filename, "analysis.ipynb");
    assert_eq!(gist.files[0].size, content.len() as u64);
    assert!(
        gist.files[0]
            .raw_url
            .as_deref()
            .unwrap()
            .ends_with("/analysis.ipynb")
    );
    assert_eq!(gist.notebook_file().unwrap().filename, "analysis.ipynb");

    let updated = client
        .update_gist(
            &gist.id,
            &GistUpdate {
                filename: "analysis.ipynb",
                content: "{}",
                description: None,
                rename_from: None,
            },
        )
        .unwrap();
    assert_eq!(updated.id, gist.id);
    assert_eq!(updated.revisions, 2);
    let body = gh.server.requests().last().unwrap().json();
    assert_eq!(
        body,
        json!({"files": {"analysis.ipynb": {"content": "{}"}}}),
        "description omitted = unchanged"
    );

    let renamed = client
        .update_gist(
            &gist.id,
            &GistUpdate {
                filename: "final.ipynb",
                content: "{1}",
                description: Some("Final"),
                rename_from: Some("analysis.ipynb"),
            },
        )
        .unwrap();
    let body = gh.server.requests().last().unwrap().json();
    assert_eq!(
        body,
        json!({"description": "Final", "files": {"analysis.ipynb": {"filename": "final.ipynb", "content": "{1}"}}})
    );
    assert_eq!(
        renamed
            .files
            .iter()
            .map(|f| f.filename.as_str())
            .collect::<Vec<_>>(),
        ["final.ipynb"]
    );

    let fetched = client.get_gist(&gist.id).unwrap();
    assert_eq!(fetched.revisions, 3);
    assert_eq!(fetched.updated_at, "2026-10-16T09:03:00Z");
    assert!(parse_timestamp(&fetched.updated_at).is_some());

    client.delete_gist(&gist.id).unwrap();
    assert_eq!(client.get_gist(&gist.id), Err(ShareError::NotFound));
    assert_eq!(client.delete_gist(&gist.id), Err(ShareError::NotFound));

    // Ids are validated before anything is sent.
    let before = gh.server.requests().len();
    assert!(matches!(
        client.get_gist("../user"),
        Err(ShareError::InvalidInput(_))
    ));
    assert!(matches!(
        client.list_comments("a/b"),
        Err(ShareError::InvalidInput(_))
    ));
    assert_eq!(gh.server.requests().len(), before);
}

#[test]
fn comments_crud_and_pagination() {
    let gh = FakeGitHub::start("octocat");
    let client = gh.client();
    let id = gh.insert("octocat", true, &[("nb.ipynb", "{}")]);
    {
        let mut st = gh.state.lock().unwrap();
        let g = st.gists.get_mut(&id).unwrap();
        for i in 0..230 {
            g.comments.push(FakeComment {
                id: 1000 + i,
                author: "reader".into(),
                body: format!("comment {i}"),
            });
        }
    }
    let comments = client.list_comments(&id).unwrap();
    assert_eq!(comments.len(), 230);
    assert_eq!(comments[0].body, "comment 0");
    assert_eq!(comments[229].body, "comment 229");
    assert_eq!(comments[0].author.as_deref(), Some("reader"));
    assert_eq!(comments[0].author_association.as_deref(), Some("NONE"));
    let pages: Vec<_> = gh
        .server
        .requests()
        .iter()
        .map(|r| r.query("page").map(str::to_string))
        .collect();
    assert_eq!(pages, [None, Some("2".into()), Some("3".into())]);
    assert!(
        gh.server
            .requests()
            .iter()
            .all(|r| r.query("per_page") == Some("100"))
    );

    let mine = client.create_comment(&id, "Nice **plot**").unwrap();
    assert_eq!(mine.body, "Nice **plot**");
    assert_eq!(mine.author.as_deref(), Some("octocat"));
    assert_eq!(
        gh.server.requests().last().unwrap().json(),
        json!({"body": "Nice **plot**"})
    );
    let edited = client.update_comment(&id, mine.id, "Nice plot!").unwrap();
    assert_eq!((edited.id, edited.body.as_str()), (mine.id, "Nice plot!"));
    client.delete_comment(&id, mine.id).unwrap();
    assert_eq!(client.list_comments(&id).unwrap().len(), 230);
    assert_eq!(
        client.delete_comment(&id, mine.id),
        Err(ShareError::NotFound)
    );

    let before = gh.server.requests().len();
    assert!(matches!(
        client.create_comment(&id, "  \n"),
        Err(ShareError::InvalidInput(_))
    ));
    assert_eq!(
        gh.server.requests().len(),
        before,
        "empty comments aren't sent"
    );

    // No comments: one request, empty list.
    let empty = gh.insert("octocat", true, &[("x.ipynb", "{}")]);
    assert_eq!(client.list_comments(&empty).unwrap(), vec![]);
}

#[test]
fn pagination_never_sends_the_token_to_another_host() {
    let foreign = MockServer::start(|_| Reply::json(200, json!([])));
    let foreign_url = foreign.url();
    let comment = |i: u64| json!({"id": i, "body": format!("c{i}"), "user": null, "created_at": "", "updated_at": ""});
    let server = MockServer::start(move |req| match req.query("page") {
        None => Reply::json(200, json!((0..100).map(comment).collect::<Vec<_>>())).header(
            "Link",
            &format!("<{foreign_url}/gists/abc/comments?page=2>; rel=\"next\""),
        ),
        Some("2") => Reply::json(200, json!([comment(100)])),
        Some(_) => Reply::json(500, json!({"message": "unexpected page"})),
    });
    let comments = client_for(&server).list_comments("abc").unwrap();
    assert_eq!(comments.len(), 101);
    assert_eq!(comments[100].author, None, "deleted account");
    assert!(
        foreign.requests().is_empty(),
        "followed a Link to another host"
    );
    assert_eq!(
        server.requests()[1].path,
        "/gists/abc/comments?per_page=100&page=2"
    );
}

fn error_for(reply: Reply, call: impl FnOnce(&GistClient) -> Result<()>) -> ShareError {
    let server = MockServer::start(move |_| reply.clone());
    call(&client_for(&server)).unwrap_err()
}

fn create(c: &GistClient) -> Result<()> {
    c.create_gist(&NewGist {
        filename: "a.ipynb",
        content: "{}",
        description: "",
        public: true,
    })
    .map(drop)
}

#[test]
fn http_errors_are_typed() {
    assert_eq!(
        error_for(
            Reply::json(401, json!({"message": "Bad credentials"})),
            |c| c.whoami().map(drop)
        ),
        ShareError::BadToken
    );
    // Classic token without the gist scope: 403, or 404 as GitHub sometimes answers.
    let e = error_for(
        Reply::json(403, json!({"message": "Forbidden"})).header("X-OAuth-Scopes", "repo"),
        create,
    );
    assert_eq!(
        e,
        ShareError::MissingGistScope {
            scopes: Some(vec!["repo".into()])
        }
    );
    assert!(e.is_auth_problem());
    assert!(e.to_string().contains("gist"));
    let e = error_for(
        Reply::json(404, json!({"message": "Not Found"}))
            .header("X-OAuth-Scopes", "read:org")
            .header("X-Accepted-OAuth-Scopes", "gist"),
        create,
    );
    assert_eq!(
        e,
        ShareError::MissingGistScope {
            scopes: Some(vec!["read:org".into()])
        }
    );
    // Fine-grained token without the Gists permission.
    let e = error_for(
        Reply::json(
            403,
            json!({"message": "Resource not accessible by personal access token"}),
        ),
        |c| {
            c.update_gist(
                "abc",
                &GistUpdate {
                    filename: "a.ipynb",
                    content: "{}",
                    description: None,
                    rename_from: None,
                },
            )
            .map(drop)
        },
    );
    assert_eq!(e, ShareError::MissingGistScope { scopes: None });

    assert_eq!(
        error_for(
            Reply::json(404, json!({"message": "Not Found"})).header("X-OAuth-Scopes", "gist"),
            |c| c.get_gist("abc").map(drop)
        ),
        ShareError::NotFound
    );
    let e = error_for(
        Reply::json(
            422,
            json!({"message": "Validation Failed", "errors": [{"resource": "Gist", "code": "missing_field", "field": "files"}]}),
        ),
        create,
    );
    assert_eq!(
        e,
        ShareError::Rejected {
            message: "Validation Failed (files: missing_field)".into()
        }
    );

    // Primary rate limit: reset time in the headers.
    let reset = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    let e = error_for(
        Reply::json(
            403,
            json!({"message": "API rate limit exceeded for user ID 1."}),
        )
        .header("X-RateLimit-Limit", "5000")
        .header("X-RateLimit-Remaining", "0")
        .header("X-RateLimit-Reset", &reset.to_string()),
        |c| c.list_comments("abc").map(drop),
    );
    assert_eq!(
        e,
        ShareError::RateLimited {
            reset_at: Some(reset),
            retry_after: None
        }
    );
    assert!(e.is_retryable());
    let wait = e.wait_secs(SystemTime::now()).unwrap();
    assert!((590..=600).contains(&wait), "{wait}");
    assert!(e.to_string().contains("in 10 minutes"), "{e}");
    // Secondary rate limits: 403 or 429 with Retry-After.
    assert_eq!(
        error_for(
            Reply::json(429, json!({"message": "Too many requests"})).header("Retry-After", "30"),
            create
        ),
        ShareError::RateLimited {
            reset_at: None,
            retry_after: Some(30)
        }
    );
    assert_eq!(
        error_for(
            Reply::json(
                403,
                json!({"message": "You have exceeded a secondary rate limit."})
            )
            .header("Retry-After", "60"),
            create
        ),
        ShareError::RateLimited {
            reset_at: None,
            retry_after: Some(60)
        }
    );

    let e = error_for(Reply::json(502, json!({"message": "Server Error"})), create);
    assert_eq!(
        e,
        ShareError::Http {
            status: 502,
            message: "Server Error".into()
        }
    );
    assert!(e.is_retryable());
    assert!(matches!(
        error_for(Reply::raw(200, "<html>proxy login</html>"), create),
        ShareError::BadResponse(_)
    ));
}

#[test]
fn network_failures_are_typed() {
    // Nothing listening.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = GistClient::with_base_url(
        Token::new(FAKE_TOKEN).unwrap(),
        &format!("http://127.0.0.1:{port}"),
    );
    match client.whoami() {
        Err(ShareError::Network {
            timed_out: false, ..
        }) => {}
        other => panic!("{other:?}"),
    }

    // A server that answers too late.
    let slow = MockServer::start(|_| {
        Reply::json(200, json!({"login": "x"})).delay(Duration::from_secs(3))
    });
    let timeouts = Timeouts {
        connect: Duration::from_secs(2),
        response: Duration::from_millis(300),
        upload: Duration::from_secs(2),
    };
    let client = GistClient::with_options(Token::new(FAKE_TOKEN).unwrap(), &slow.url(), timeouts);
    let e = client.whoami().unwrap_err();
    assert_eq!(
        e,
        ShareError::Network {
            message: "timed out".into(),
            timed_out: true
        }
    );
    assert!(e.is_retryable());

    // Plain http is refused for anything but loopback (the token would travel in clear).
    // ureq checks the scheme before resolving or connecting.
    let client =
        GistClient::with_base_url(Token::new(FAKE_TOKEN).unwrap(), "http://api.github.invalid");
    assert!(matches!(client.whoami(), Err(ShareError::InvalidInput(_))));
}

#[test]
fn publish_creates_then_updates_the_same_gist() {
    let gh = FakeGitHub::start("octocat");
    let client = gh.client();
    let mut nb = sample_notebook();

    let first = publish(
        &client,
        &mut nb,
        &PublishOptions::new("/Users/me/Results.ipynb", false),
    )
    .unwrap();
    assert!(first.created);
    assert_eq!(
        gh.server.log(),
        ["POST /gists"],
        "a first publish is one request"
    );
    let info = read_share_info(&nb).unwrap();
    assert_eq!(info, first.info);
    assert_eq!(info.gist_id, first.gist.id);
    assert!(!info.public);
    assert_eq!(info.filename.as_deref(), Some("Results.ipynb"));
    assert_eq!(info.owner.as_deref(), Some("octocat"));
    assert_eq!(
        info.html_url,
        format!("https://gist.github.com/{}", info.gist_id)
    );
    assert_eq!(info.published_at, "2026-10-16T09:01:00Z");
    assert_eq!(
        info.nbviewer_url(),
        format!("https://nbviewer.org/gist/{}", info.gist_id)
    );

    let remote = gh.gist(&info.gist_id).unwrap();
    assert_eq!(remote.description, "Results");
    let uploaded = herse_nb::parse(remote.files["Results.ipynb"].as_bytes()).unwrap();
    assert_eq!(uploaded.cells.len(), 2);
    assert_eq!(uploaded.cells[1].outputs.len(), 1);
    assert_eq!(
        read_share_info(&uploaded),
        None,
        "the published copy doesn't point at the gist"
    );

    // Edit and publish again: a new revision of the same gist.
    nb.cells[1].source = "print(6 * 9)".into();
    let second = publish(&client, &mut nb, &PublishOptions::new("Results", false)).unwrap();
    assert!(!second.created);
    assert_eq!(second.info.gist_id, info.gist_id);
    assert_eq!(second.gist.revisions, 2);
    assert_eq!(gh.state.lock().unwrap().gists.len(), 1);
    let id = &info.gist_id;
    assert_eq!(
        gh.server.log()[1..],
        [
            format!("GET /gists/{id}"),
            "GET /user".into(),
            format!("PATCH /gists/{id}")
        ]
    );
    assert!(gh.gist(id).unwrap().files["Results.ipynb"].contains("6 * 9"));

    // Renamed notebook, outputs stripped: same gist, file renamed; the login is cached.
    let opts = PublishOptions {
        strip_outputs: true,
        ..PublishOptions::new("Final.ipynb", false)
    };
    let third = publish(&client, &mut nb, &opts).unwrap();
    assert!(!third.created);
    assert_eq!(
        gh.server.log()[4..],
        [format!("GET /gists/{id}"), format!("PATCH /gists/{id}")]
    );
    let remote = gh.gist(id).unwrap();
    assert_eq!(remote.files.keys().collect::<Vec<_>>(), ["Final.ipynb"]);
    assert_eq!(remote.revisions, 3);
    let uploaded = herse_nb::parse(remote.files["Final.ipynb"].as_bytes()).unwrap();
    assert!(uploaded.cells[1].outputs.is_empty());
    assert_eq!(uploaded.cells[1].execution_count, None);
    assert_eq!(
        nb.cells[1].outputs.len(),
        1,
        "the open notebook keeps its outputs"
    );
    assert_eq!(
        read_share_info(&nb).unwrap().filename.as_deref(),
        Some("Final.ipynb")
    );
}

#[test]
fn publish_recovers_from_deleted_or_foreign_gists() {
    let gh = FakeGitHub::start("octocat");
    let client = gh.client();
    let mut nb = sample_notebook();
    let first = publish(&client, &mut nb, &PublishOptions::new("nb", true)).unwrap();

    // Deleted on github.com: a new gist, new links.
    gh.state.lock().unwrap().gists.clear();
    let again = publish(&client, &mut nb, &PublishOptions::new("nb", true)).unwrap();
    assert!(again.created);
    assert_ne!(again.info.gist_id, first.info.gist_id);
    assert_eq!(read_share_info(&nb).unwrap().gist_id, again.info.gist_id);

    // A notebook someone else published (their metadata came with the file).
    let alices = gh.insert("alice", true, &[("nb.ipynb", "{}")]);
    let mut theirs = sample_notebook();
    write_share_info(
        &mut theirs,
        &ShareInfo {
            gist_id: alices.clone(),
            public: true,
            html_url: gist_html_url(&alices),
            published_at: String::new(),
            filename: None,
            owner: Some("alice".into()),
        },
    );
    let mine = publish(&client, &mut theirs, &PublishOptions::new("nb", true)).unwrap();
    assert!(mine.created);
    assert_ne!(mine.info.gist_id, alices);
    assert_eq!(mine.info.owner.as_deref(), Some("octocat"));
    assert_eq!(
        gh.gist(&alices).unwrap().revisions,
        1,
        "alice's gist untouched"
    );
}

#[test]
fn publish_refuses_to_change_visibility() {
    let gh = FakeGitHub::start("octocat");
    let client = gh.client();
    let mut nb = sample_notebook();
    let secret = publish(&client, &mut nb, &PublishOptions::new("nb", false)).unwrap();
    let requests = gh.server.requests().len();

    let e = publish(&client, &mut nb, &PublishOptions::new("nb", true)).unwrap_err();
    assert_eq!(e, ShareError::VisibilityMismatch { gist_public: false });
    assert!(!e.to_string().is_empty());
    let log = gh.server.log();
    assert!(
        log[requests..].iter().all(|r| r.starts_with("GET ")),
        "nothing uploaded: {log:?}"
    );
    assert_eq!(read_share_info(&nb).unwrap(), secret.info);

    // Asking for a new gist makes a separate public one; the secret one stays.
    let opts = PublishOptions {
        new_gist: true,
        ..PublishOptions::new("nb", true)
    };
    let public = publish(&client, &mut nb, &opts).unwrap();
    assert!(public.created && public.info.public);
    assert_ne!(public.info.gist_id, secret.info.gist_id);
    assert!(gh.gist(&secret.info.gist_id).is_some());
    assert_eq!(read_share_info(&nb).unwrap().gist_id, public.info.gist_id);
}

#[test]
fn publish_errors_leave_the_notebook_alone() {
    let server = MockServer::start(|_| {
        Reply::json(
            403,
            json!({"message": "Resource not accessible by personal access token"}),
        )
    });
    let mut nb = sample_notebook();
    let before = nb.clone();
    let e = publish(
        &client_for(&server),
        &mut nb,
        &PublishOptions::new("nb", false),
    )
    .unwrap_err();
    assert_eq!(e, ShareError::MissingGistScope { scopes: None });
    assert_eq!(nb, before);
}

#[test]
fn unpublish_deletes_and_forgets() {
    let gh = FakeGitHub::start("octocat");
    let client = gh.client();
    let mut nb = sample_notebook();
    assert_eq!(unpublish(&client, &mut nb), Ok(None));
    assert!(gh.server.requests().is_empty());

    let published = publish(&client, &mut nb, &PublishOptions::new("nb", false)).unwrap();
    let removed = unpublish(&client, &mut nb).unwrap().unwrap();
    assert_eq!(removed.gist_id, published.info.gist_id);
    assert!(gh.gist(&removed.gist_id).is_none());
    assert_eq!(read_share_info(&nb), None);
    assert!(!nb.metadata.contains_key("herse"));

    // Already deleted remotely: still forgets it.
    publish(&client, &mut nb, &PublishOptions::new("nb", false)).unwrap();
    gh.state.lock().unwrap().gists.clear();
    assert!(unpublish(&client, &mut nb).unwrap().is_some());
    assert_eq!(read_share_info(&nb), None);
}
