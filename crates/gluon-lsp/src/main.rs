//! Gluon LSP binary.
//!
//! Uses `lsp-server` for stdio transport. The event loop is a single
//! thread reading messages off the receiver and dispatching by method
//! name — no async runtime, no worker pool. Everything we serve is
//! answered from an in-memory [`DslIndex`] (built once at startup)
//! and a [`HashMap`] of open document contents, so request handling
//! is synchronous and sub-millisecond.
//!
//! ### Phase 1 (this binary)
//!
//! - `initialize` — advertise completion + hover + full-text sync.
//! - `textDocument/didOpen` / `didChange` / `didClose` — track buffer
//!   contents in memory. We request `TextDocumentSyncKind::FULL`, so
//!   every didChange notification carries the full buffer as a single
//!   content change.
//! - `textDocument/completion` — return every registered DSL function.
//! - `textDocument/hover` — signature for the identifier under cursor.
//!
//! ### Phase 2 (explicitly not here)
//!
//! AST-aware completion/hover scoping, Rhai parse diagnostics, and
//! go-to-definition. Those require parsing the buffer with
//! `rhai::Engine::compile` and walking the AST; keeping the current
//! file free of that complexity is deliberate.

mod completion;
mod hover;
mod index;
mod word;

use anyhow::{Context, Result};
use index::DslIndex;
use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CompletionOptions, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, HoverProviderCapability, InitializeParams, OneOf,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use std::collections::HashMap;

fn main() -> Result<()> {
    // stdio is the only transport we support. Editors launch us as a
    // child process and pipe LSP traffic over stdin/stdout.
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        // Full-text sync simplifies buffer tracking: every didChange
        // carries the whole document, so we don't need an incremental
        // delta applier. The buffers are small (gluon.rhai files are
        // typically <1KB) so the bandwidth cost is irrelevant.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::FULL,
        )),
        completion_provider: Some(CompletionOptions {
            // No trigger characters: the client asks for completions
            // on Ctrl-Space (or the user's configured key), not on
            // every keystroke. That keeps us out of the hot path until
            // Phase 2 adds real scoping.
            trigger_characters: None,
            all_commit_characters: None,
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
            completion_item: None,
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(false)),
        ..Default::default()
    };

    let init_params = connection
        .initialize(serde_json::to_value(capabilities).context("serialize capabilities")?)
        .context("LSP initialize handshake failed")?;
    let _init: InitializeParams =
        serde_json::from_value(init_params).context("parse InitializeParams")?;

    // Build the symbol index exactly once. Subsequent requests all
    // read from this shared state.
    let index = DslIndex::from_engine();
    let docs: HashMap<Url, String> = HashMap::new();

    main_loop(connection, index, docs)?;

    io_threads.join().context("LSP io thread join failed")?;
    Ok(())
}

/// The request/notification pump.
///
/// Takes the [`Connection`] by value so that on exit we can `drop`
/// it — releasing both the reader-thread receiver and the
/// writer-thread sender. Without that drop, `io_threads.join()` on
/// the caller side would block forever waiting for threads whose
/// channels we're still holding open.
fn main_loop(
    connection: Connection,
    index: DslIndex,
    mut docs: HashMap<Url, String>,
) -> Result<()> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection
                    .handle_shutdown(&req)
                    .context("handle_shutdown failed")?
                {
                    // Shutdown request fully handled (response was
                    // sent by handle_shutdown). Fall through so the
                    // loop exits when the client closes the
                    // connection; the next message — typically an
                    // `exit` notification — will either arrive and
                    // be ignored, or EOF will end the iterator.
                    break;
                }
                handle_request(&connection, &index, &docs, req)?;
            }
            Message::Notification(not) => {
                handle_notification(&mut docs, not)?;
            }
            // Responses to server-initiated requests — we don't
            // initiate any, so there's nothing to correlate. Silently
            // ignore.
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn handle_request(
    connection: &Connection,
    index: &DslIndex,
    docs: &HashMap<Url, String>,
    req: Request,
) -> Result<()> {
    match req.method.as_str() {
        "textDocument/completion" => {
            let (id, params) = cast_request::<lsp_types::request::Completion>(req)?;
            let uri = params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;
            let doc = docs.get(&uri).map(String::as_str).unwrap_or("");
            let resp = completion::complete(index, doc, pos);
            send_result(connection, id, resp)?;
        }
        "textDocument/hover" => {
            let (id, params) = cast_request::<lsp_types::request::HoverRequest>(req)?;
            let uri = params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;
            let doc = docs.get(&uri).map(String::as_str).unwrap_or("");
            let resp = hover::hover(index, doc, pos);
            send_result(connection, id, resp)?;
        }
        _ => {
            // Methods we don't implement get an empty result rather
            // than a MethodNotFound error. Some clients treat
            // MethodNotFound as fatal and disconnect, which is worse
            // than just replying "nothing to show".
            send_result(connection, req.id, serde_json::Value::Null)?;
        }
    }
    Ok(())
}

fn handle_notification(docs: &mut HashMap<Url, String>, not: Notification) -> Result<()> {
    match not.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = serde_json::from_value(not.params)
                .context("parse DidOpenTextDocumentParams")?;
            docs.insert(params.text_document.uri, params.text_document.text);
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = serde_json::from_value(not.params)
                .context("parse DidChangeTextDocumentParams")?;
            // Full sync: there is exactly one content change and it
            // carries the complete new buffer. If a client sends
            // incremental changes anyway (spec violation given our
            // advertised capability), we take the last entry's text
            // as best-effort — it's the most recent edit.
            if let Some(change) = params.content_changes.into_iter().last() {
                docs.insert(params.text_document.uri, change.text);
            }
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams = serde_json::from_value(not.params)
                .context("parse DidCloseTextDocumentParams")?;
            docs.remove(&params.text_document.uri);
        }
        // Any other notification (initialized, cancelRequest, …) is
        // either handshake noise or state we don't track. Dropping
        // them silently is the right default.
        _ => {}
    }
    Ok(())
}

/// Extract the typed params from a raw [`Request`]. Returns a clear
/// error rather than panicking so the LSP loop can keep going if a
/// single malformed request arrives.
fn cast_request<R>(req: Request) -> Result<(RequestId, R::Params)>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD).map_err(|e| match e {
        ExtractError::JsonError { method, error } => {
            anyhow::anyhow!("failed to parse `{method}` params: {error}")
        }
        ExtractError::MethodMismatch(req) => {
            anyhow::anyhow!("method mismatch: expected `{}`, got `{}`", R::METHOD, req.method)
        }
    })
}

fn send_result<T: serde::Serialize>(
    connection: &Connection,
    id: RequestId,
    result: T,
) -> Result<()> {
    let resp = Response {
        id,
        result: Some(serde_json::to_value(result).context("serialize LSP response")?),
        error: None,
    };
    connection
        .sender
        .send(Message::Response(resp))
        .context("send LSP response")?;
    Ok(())
}
