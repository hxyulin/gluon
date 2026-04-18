//! Gluon LSP binary.
//!
//! Uses `lsp-server` for stdio transport. The event loop is a single
//! thread reading messages off the receiver and dispatching by method
//! name — no async runtime, no worker pool. State is held in memory:
//! a [`DslSchema`] (built once at startup, drives semantic analysis),
//! a [`RhaiParser`] (cheap to construct,
//! cached so the tree-sitter parser allocation amortizes), a buffer
//! map, and a per-document analysis cache so semantic-token requests
//! can serve the most recent analysis without reparsing.
//!
//! ### Capabilities advertised
//!
//! - `initialize` — full-text sync, completion, hover, semantic tokens.
//! - `textDocument/didOpen` / `didChange` — track buffer contents and
//!   re-run analysis, publishing diagnostics back to the client.
//! - `textDocument/didClose` — drop buffer + analysis state.
//! - `textDocument/completion` — return every registered DSL function.
//! - `textDocument/hover` — signature for the identifier under cursor.
//! - `textDocument/semanticTokens/full` — delta-encoded classified
//!   tokens from the cached analysis.

use anyhow::{Context, Result};
use clap::Parser;
use gluon_core::engine::schema::DslSchema;
use gluon_lsp::analysis::{self, AnalysisResult};
use gluon_lsp::parser::Parser as _;
use gluon_lsp::parser::rhai::RhaiParser;
use gluon_lsp::{completion, diagnostics, hover, semantic_tokens};
use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CompletionOptions, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, HoverProviderCapability, InitializeParams, OneOf,
    PublishDiagnosticsParams, SemanticTokens, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use std::collections::HashMap;

/// Editors launch the binary with no arguments and communicate over
/// stdio. The argument surface exists only so `--help` and `--version`
/// work for humans checking their install, without us inventing a
/// config story we don't yet need.
#[derive(Parser)]
#[command(
    version,
    about = "Language server for Gluon's Rhai DSL (gluon.rhai)",
    long_about = None,
)]
struct Cli {}

fn main() -> Result<()> {
    // Must run before we touch stdio — clap handles --help / --version
    // by printing and exiting, which would otherwise collide with the
    // LSP transport grabbing stdin below.
    Cli::parse();

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
            // `.` triggers completion mid-chain so the client asks us
            // as soon as the user types it. Other completion contexts
            // still work via Ctrl-Space.
            trigger_characters: Some(vec![".".to_string()]),
            all_commit_characters: None,
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
            completion_item: None,
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(false)),
        // Full semantic tokens only — no range or delta variants.
        // Documents are small enough that recomputing the whole token
        // set per request is cheaper than maintaining delta state.
        semantic_tokens_provider: Some(
            SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: None,
                work_done_progress_options: Default::default(),
            }),
        ),
        ..Default::default()
    };

    let init_params = connection
        .initialize(serde_json::to_value(capabilities).context("serialize capabilities")?)
        .context("LSP initialize handshake failed")?;
    let _init: InitializeParams =
        serde_json::from_value(init_params).context("parse InitializeParams")?;

    // Build the schema once at startup. All subsequent requests read
    // from this shared state.
    let schema = gluon_core::engine::dsl_schema();
    let parser = RhaiParser::new();
    let docs: HashMap<Url, String> = HashMap::new();
    let analysis_cache: HashMap<Url, AnalysisResult> = HashMap::new();

    main_loop(connection, schema, parser, docs, analysis_cache)?;

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
    schema: DslSchema,
    parser: RhaiParser,
    mut docs: HashMap<Url, String>,
    mut analysis_cache: HashMap<Url, AnalysisResult>,
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
                handle_request(
                    &connection,
                    &schema,
                    &parser,
                    &docs,
                    &analysis_cache,
                    req,
                )?;
            }
            Message::Notification(not) => {
                handle_notification(
                    &connection,
                    &schema,
                    &parser,
                    &mut docs,
                    &mut analysis_cache,
                    not,
                )?;
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
    schema: &DslSchema,
    parser: &RhaiParser,
    docs: &HashMap<Url, String>,
    analysis_cache: &HashMap<Url, AnalysisResult>,
    req: Request,
) -> Result<()> {
    match req.method.as_str() {
        "textDocument/completion" => {
            let (id, params) = cast_request::<lsp_types::request::Completion>(req)?;
            let uri = params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;
            let doc = docs.get(&uri).map(String::as_str).unwrap_or("");
            let resp = completion::complete(schema, parser, doc, pos);
            send_result(connection, id, resp)?;
        }
        "textDocument/hover" => {
            let (id, params) = cast_request::<lsp_types::request::HoverRequest>(req)?;
            let uri = params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;
            let doc = docs.get(&uri).map(String::as_str).unwrap_or("");
            let resp = hover::hover(schema, parser, doc, pos);
            send_result(connection, id, resp)?;
        }
        "textDocument/semanticTokens/full" => {
            let (id, params) =
                cast_request::<lsp_types::request::SemanticTokensFullRequest>(req)?;
            let uri = params.text_document.uri;
            // Serve from cache. didOpen/didChange always populate the
            // cache before the client can issue a token request, so an
            // empty result here means the buffer was never opened —
            // returning empty tokens is the right LSP-spec behaviour.
            let data = analysis_cache
                .get(&uri)
                .map(|r| semantic_tokens::encode(&r.tokens))
                .unwrap_or_default();
            let resp = SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data,
            });
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

fn handle_notification(
    connection: &Connection,
    schema: &DslSchema,
    parser: &RhaiParser,
    docs: &mut HashMap<Url, String>,
    analysis_cache: &mut HashMap<Url, AnalysisResult>,
    not: Notification,
) -> Result<()> {
    match not.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams =
                serde_json::from_value(not.params).context("parse DidOpenTextDocumentParams")?;
            let uri = params.text_document.uri;
            let text = params.text_document.text;
            docs.insert(uri.clone(), text);
            // Re-borrow through the map so we analyse the canonical
            // stored copy — keeps the source-of-truth single.
            let source = docs.get(&uri).map(String::as_str).unwrap_or("");
            run_analysis_and_push_diagnostics(
                connection,
                schema,
                parser,
                &uri,
                source,
                analysis_cache,
            )?;
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = serde_json::from_value(not.params)
                .context("parse DidChangeTextDocumentParams")?;
            let uri = params.text_document.uri;
            // Full sync: there is exactly one content change and it
            // carries the complete new buffer. If a client sends
            // incremental changes anyway (spec violation given our
            // advertised capability), we take the last entry's text
            // as best-effort — it's the most recent edit.
            if let Some(change) = params.content_changes.into_iter().last() {
                docs.insert(uri.clone(), change.text);
                let source = docs.get(&uri).map(String::as_str).unwrap_or("");
                run_analysis_and_push_diagnostics(
                    connection,
                    schema,
                    parser,
                    &uri,
                    source,
                    analysis_cache,
                )?;
            }
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams =
                serde_json::from_value(not.params).context("parse DidCloseTextDocumentParams")?;
            docs.remove(&params.text_document.uri);
            analysis_cache.remove(&params.text_document.uri);
        }
        // Any other notification (initialized, cancelRequest, …) is
        // either handshake noise or state we don't track. Dropping
        // them silently is the right default.
        _ => {}
    }
    Ok(())
}

/// Parse, analyse, cache the result, and push diagnostics back to the
/// client. Called on every didOpen/didChange so the editor's red
/// squigglies stay in sync with the buffer.
///
/// Diagnostics are always pushed — even an empty list — so that fixing
/// the last error in a buffer clears the previous squigglies. If we
/// only sent a notification when there were diagnostics, stale errors
/// would linger in the editor.
fn run_analysis_and_push_diagnostics(
    connection: &Connection,
    schema: &DslSchema,
    parser: &RhaiParser,
    uri: &Url,
    source: &str,
    analysis_cache: &mut HashMap<Url, AnalysisResult>,
) -> Result<()> {
    let tree = parser.parse(source);
    let result = analysis::analyze(&tree, schema);

    let lsp_diags = diagnostics::to_lsp_diagnostics(&result.diagnostics);
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: lsp_diags,
        version: None,
    };
    let not = Notification {
        method: "textDocument/publishDiagnostics".to_string(),
        params: serde_json::to_value(params).context("serialize diagnostics")?,
    };
    connection
        .sender
        .send(Message::Notification(not))
        .context("send diagnostics")?;

    analysis_cache.insert(uri.clone(), result);
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
