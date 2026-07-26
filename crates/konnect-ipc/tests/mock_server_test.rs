//! IPC client tests against a mock KiCAD NNG server — no KiCAD required.
//!
//! A rep0 socket on tcp://127.0.0.1:<port> plays KiCAD: it decodes the
//! ApiRequest envelope and returns canned ApiResponse messages. This lets CI
//! exercise the full encode → transport → decode → error-mapping path that
//! previously only ran against a live KiCAD session.

use konnect_ipc::builders;
use konnect_ipc::gen::kiapi;
use konnect_ipc::KiCadIpcClient;
use nng::options::Options;
use prost::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A rep0 server answering each request via `respond`.
/// Returns the tcp:// URL to dial. The server thread exits when the socket
/// errors (i.e. when `_socket_keepalive` is dropped by the returned guard).
struct MockKicad {
    url: String,
    _thread: std::thread::JoinHandle<()>,
}

fn spawn_mock<F>(respond: F) -> MockKicad
where
    F: Fn(kiapi::common::ApiRequest) -> Option<kiapi::common::ApiResponse> + Send + 'static,
{
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("tcp://127.0.0.1:{port}");

    // Listen BEFORE returning (and before the receive thread spawns): the
    // client dials the moment spawn_mock returns, and NNG's dial fails
    // immediately with Connection refused if nothing is bound yet. Doing the
    // listen inside the thread raced the caller — flaky on slow CI runners.
    let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
    socket
        .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(20)))
        .unwrap();
    socket.listen(&url).expect("mock listen");

    let thread = std::thread::spawn(move || {
        while let Ok(msg) = socket.recv() {
            let request = match kiapi::common::ApiRequest::decode(msg.as_slice()) {
                Ok(r) => r,
                Err(_) => break,
            };
            match respond(request) {
                Some(resp) => {
                    let out = nng::Message::from(resp.encode_to_vec().as_slice());
                    if socket.send(out).is_err() {
                        break;
                    }
                }
                None => {
                    // Simulate a wedged KiCAD: never reply. The rep socket
                    // can't take another request until it replies, so just
                    // park until the test ends.
                    std::thread::sleep(Duration::from_secs(20));
                    break;
                }
            }
        }
    });

    MockKicad {
        url,
        _thread: thread,
    }
}

fn ok_response() -> kiapi::common::ApiResponse {
    kiapi::common::ApiResponse {
        status: Some(kiapi::common::ApiResponseStatus {
            status: kiapi::common::ApiStatusCode::AsOk as i32,
            error_message: String::new(),
        }),
        header: None,
        message: None,
    }
}

fn reply_with(inner: prost_types::Any) -> kiapi::common::ApiResponse {
    kiapi::common::ApiResponse {
        message: Some(inner),
        ..ok_response()
    }
}

fn open_board_response() -> kiapi::common::ApiResponse {
    let response = kiapi::common::commands::GetOpenDocumentsResponse {
        documents: vec![kiapi::common::types::DocumentSpecifier {
            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
            project: None,
            identifier: Some(
                kiapi::common::types::document_specifier::Identifier::BoardFilename(
                    "test.kicad_pcb".to_string(),
                ),
            ),
        }],
    };
    reply_with(builders::pack_any(
        &response,
        "kiapi.common.commands.GetOpenDocumentsResponse",
    ))
}

#[test]
fn ping_roundtrips_through_mock() {
    let mock = spawn_mock(|req| {
        // The envelope must carry a client name and a packed command.
        assert!(req.header.is_some());
        let header = req.header.unwrap();
        assert!(header.client_name.starts_with("konnect-"));
        let msg = req.message.expect("request must pack a command");
        assert!(
            msg.type_url.ends_with("kiapi.common.commands.Ping"),
            "unexpected type_url: {}",
            msg.type_url
        );
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    assert!(client.ping().unwrap());
}

#[test]
fn explicit_kicad_token_is_sent_in_request_header() {
    let mock = spawn_mock(|req| {
        let header = req.header.expect("request header");
        assert_eq!(header.kicad_token, "linux-instance-token");
        Some(ok_response())
    });

    let client = KiCadIpcClient::new_with_token(&mock.url, "linux-instance-token");
    assert!(client.ping().unwrap());
}

#[test]
fn kicad_error_status_maps_to_err() {
    let mock = spawn_mock(|_req| {
        Some(kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                error_message: "no board open".to_string(),
            }),
            header: None,
            message: None,
        })
    });

    let client = KiCadIpcClient::new(&mock.url);
    // ping() swallows errors into Ok(false) by design — that's the
    // "KiCAD unreachable" UX. It must not be Ok(true) and must not hang.
    assert!(!client.ping().unwrap());

    // A typed call surfaces the error text.
    let err = client.get_open_documents().unwrap_err().to_string();
    assert!(err.contains("no board open"), "unexpected error: {err}");
}

#[test]
fn unreachable_endpoint_errors_fast() {
    // Nothing listens here; dial must fail with an error, not hang.
    let client = KiCadIpcClient::new("tcp://127.0.0.1:1");
    let start = std::time::Instant::now();
    let result = client.get_open_documents();
    assert!(result.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "dial to dead endpoint took {:?}",
        start.elapsed()
    );
}

#[test]
fn empty_socket_path_is_configuration_error() {
    // Clear KICAD_API_SOCKET influence by passing explicit empty and hoping
    // the env var isn't set in CI; if it is, skip.
    if std::env::var("KICAD_API_SOCKET").is_ok() {
        eprintln!("SKIP: KICAD_API_SOCKET set in environment");
        return;
    }
    let client = KiCadIpcClient::new("");
    let err = client.get_open_documents().unwrap_err().to_string();
    assert!(
        err.contains("socket path not configured"),
        "unexpected error: {err}"
    );
    assert!(
        err.contains("TROUBLESHOOTING"),
        "error should link the troubleshooting guide: {err}"
    );
}

#[test]
fn create_items_requires_a_typed_response_payload() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("CreateItems"));
            Some(ok_response())
        }
    });
    let client = KiCadIpcClient::new(&mock.url);
    let item = builders::pack_any(&kiapi::common::commands::Ping {}, "test.Item");

    let error = client.create_items(vec![item]).unwrap_err().to_string();

    assert!(error.contains("no CreateItems response payload"), "{error}");
}

#[test]
fn update_items_rejects_missing_per_item_results() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("UpdateItems"));
            let response = kiapi::common::commands::UpdateItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                updated_items: vec![],
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.UpdateItemsResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);
    let item = builders::pack_any(&kiapi::common::commands::Ping {}, "test.Item");

    let error = client.update_items(vec![item]).unwrap_err().to_string();

    assert!(
        error.contains("0 update results for 1 requested"),
        "{error}"
    );
}

#[test]
fn delete_items_surfaces_per_item_failure() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("DeleteItems"));
            let response = kiapi::common::commands::DeleteItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                deleted_items: vec![kiapi::common::commands::ItemDeletionResult {
                    id: Some(kiapi::common::types::Kiid {
                        value: "missing-id".to_string(),
                    }),
                    status: kiapi::common::commands::ItemDeletionStatus::IdsNonexistent as i32,
                }],
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.DeleteItemsResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);

    let error = client
        .delete_items(vec!["missing-id".to_string()])
        .unwrap_err()
        .to_string();

    assert!(error.contains("IDS_NONEXISTENT"), "{error}");
    assert!(error.contains("missing-id"), "{error}");
}

#[test]
fn failed_multi_step_commit_is_dropped() {
    let actions = Arc::new(Mutex::new(Vec::new()));
    let captured_actions = actions.clone();
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("BeginCommit") {
            let response = kiapi::common::commands::BeginCommitResponse {
                id: Some(kiapi::common::types::Kiid {
                    value: "commit-1".to_string(),
                }),
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.BeginCommitResponse",
            )))
        } else {
            assert!(message.type_url.ends_with("EndCommit"));
            let command =
                kiapi::common::commands::EndCommit::decode(message.value.as_slice()).unwrap();
            captured_actions.lock().unwrap().push(command.action());
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::EndCommitResponse {},
                "kiapi.common.commands.EndCommitResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);

    let error = client
        .run_commit::<()>("test transaction", |_| anyhow::bail!("second step failed"))
        .unwrap_err()
        .to_string();

    assert!(error.contains("changes dropped"), "{error}");
    assert_eq!(
        *actions.lock().unwrap(),
        vec![kiapi::common::commands::CommitAction::CmaDrop]
    );
}

/// The regression the recv timeout exists for: a server that accepts the
/// request and never replies. The predecessor project hung >600 s here; the
/// client must give up at its recv timeout instead.
///
/// Ignored by default: it necessarily takes the full 30 s recv timeout.
/// Run explicitly with: cargo test -p konnect-ipc -- --ignored
#[test]
#[ignore = "takes ~30s (full recv timeout) by design"]
fn wedged_server_times_out_instead_of_hanging() {
    let mock = spawn_mock(|_req| None); // accept, never respond

    let client = KiCadIpcClient::new(&mock.url);
    let start = std::time::Instant::now();
    let result = client.get_open_documents();
    assert!(result.is_err(), "expected timeout error");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_secs(25) && elapsed < Duration::from_secs(60),
        "expected ~30s recv timeout, got {elapsed:?}"
    );
}
