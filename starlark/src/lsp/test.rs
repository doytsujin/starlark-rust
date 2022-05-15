/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    time::Duration,
};

use gazebo::prelude::*;
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    notification::{DidChangeTextDocument, DidOpenTextDocument, Exit, Initialized, Notification},
    request::{Initialize, Request, Shutdown},
    ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams, GotoCapability,
    InitializeParams, InitializeResult, InitializedParams, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentItem, Url, VersionedTextDocumentIdentifier,
};
use serde::de::DeserializeOwned;

use crate::{
    errors::EvalMessage,
    lsp::server::{new_notification, server_with_connection, LspContext, LspEvalResult},
    syntax::{AstModule, Dialect},
};

struct TestServerContext {}

impl LspContext for TestServerContext {
    fn parse_file_with_contents(&self, filename: &str, content: String) -> LspEvalResult {
        match AstModule::parse(filename, content, &Dialect::Extended) {
            Ok(ast) => {
                let diagnostics = ast.lint(None).into_map(|l| EvalMessage::from(l).into());
                LspEvalResult {
                    diagnostics,
                    ast: Some(ast),
                }
            }
            Err(e) => {
                let diagnostics = vec![EvalMessage::from_anyhow(filename, e).into()];
                LspEvalResult {
                    diagnostics,
                    ast: None,
                }
            }
        }
    }
}

/// A server for use in testing that provides helpers for sending requests, correlating
/// responses, and sending / receiving notifications
pub struct TestServer {
    /// The thread that's actually running the server
    server_thread: Option<std::thread::JoinHandle<()>>,
    client_connection: Connection,
    /// Incrementing counter to automatically generate request IDs when making a request
    req_counter: i32,
    /// Simple incrementing document version counter
    version_counter: i32,
    /// A mapping of the requests that have arrived -> the response. Stored here as
    /// these responses might be interleaved with notifications and the like.
    responses: HashMap<RequestId, Response>,
    /// An ordered queue of all of the notifications that have been received. Drained as
    /// notifications are processed.
    notifications: VecDeque<lsp_server::Notification>,
    /// How long to wait for messages to be received.
    recv_timeout: Duration,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Need to send both a Shutdown request and an Exit notification in succession
        // so that lsp_server knows to shutdown correctly.
        let req = lsp_server::Request {
            id: self.next_request_id(),
            method: Shutdown::METHOD.to_owned(),
            params: Default::default(),
        };
        if let Err(e) = self.send_request(req) {
            eprintln!("Server was already shutdown: {}", e);
        } else {
            let notif = lsp_server::Notification {
                method: Exit::METHOD.to_owned(),
                params: Default::default(),
            };
            if let Err(e) = self.send_notification(notif) {
                eprintln!("Could not send Exit notification: {}", e);
            }
        }

        if let Some(server_thread) = self.server_thread.take() {
            server_thread.join().expect("test server to join");
        }
    }
}

impl TestServer {
    /// Generate a new request ID
    fn next_request_id(&mut self) -> RequestId {
        self.req_counter += 1;
        RequestId::from(self.req_counter)
    }

    fn next_document_version(&mut self) -> i32 {
        self.version_counter += 1;
        self.version_counter
    }

    /// Create a new request object with an automatically generated request ID.
    pub fn new_request<T: Request>(&mut self, params: T::Params) -> lsp_server::Request {
        lsp_server::Request {
            id: self.next_request_id(),
            method: T::METHOD.to_owned(),
            params: serde_json::to_value(params).unwrap(),
        }
    }

    /// Create and start a new LSP server. This sends the initialization messages, and makes
    /// sure that when the server is dropped, the threads are attempted to be stopped.
    pub(crate) fn new() -> anyhow::Result<Self> {
        let (server_connection, client_connection) = Connection::memory();

        let server_thread = std::thread::spawn(|| {
            let ctx = TestServerContext {};
            if let Err(e) = server_with_connection(server_connection, ctx) {
                eprintln!("Stopped test server thread with error `{:?}`", e);
            }
        });

        let ret = Self {
            server_thread: Some(server_thread),
            client_connection,
            req_counter: 0,
            version_counter: 0,
            responses: Default::default(),
            notifications: Default::default(),
            recv_timeout: Duration::from_secs(2),
        };
        ret.initialize()
    }

    fn initialize(mut self) -> anyhow::Result<Self> {
        let capabilities = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                definition: Some(GotoCapability {
                    dynamic_registration: Some(true),
                    link_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let init = InitializeParams {
            process_id: None,
            root_path: None,
            root_uri: None,
            initialization_options: None,
            capabilities,
            trace: None,
            workspace_folders: None,
            client_info: None,
            locale: None,
        };

        let init_request = self.new_request::<Initialize>(init);
        let initialize_id = self.send_request(init_request)?;

        self.get_response::<InitializeResult>(initialize_id)?;

        self.send_notification(lsp_server::Notification {
            method: Initialized::METHOD.to_owned(),
            params: serde_json::to_value(InitializedParams {})?,
        })?;

        Ok(self)
    }

    /// Send a request to the server, and get back the ID from the original message.
    pub fn send_request(&self, req: lsp_server::Request) -> anyhow::Result<RequestId> {
        let id = req.id.clone();
        self.send(Message::Request(req))?;
        Ok(id)
    }

    /// Send a notification to the server.
    pub fn send_notification(&self, notification: lsp_server::Notification) -> anyhow::Result<()> {
        self.send(Message::Notification(notification))
    }

    fn send(&self, message: Message) -> anyhow::Result<()> {
        Ok(self.client_connection.sender.send(message)?)
    }

    /// Receive messages from the server until either the response for the given request ID
    /// has been seen, or until there are no more messages and the receive method times out.
    pub fn get_response<T: DeserializeOwned>(&mut self, id: RequestId) -> anyhow::Result<T> {
        loop {
            self.receive()?;

            match self.responses.get(&id) {
                Some(Response {
                    error: None,
                    result: Some(result),
                    ..
                }) => {
                    break Ok(serde_json::from_value::<T>(result.clone())?);
                }
                Some(Response {
                    error: Some(err),
                    result: None,
                    ..
                }) => {
                    break Err(anyhow::anyhow!("Response error: {}", err.message));
                }
                Some(msg) => {
                    break Err(anyhow::anyhow!(
                        "Invalid response message for request {}: {:?}",
                        id,
                        msg
                    ));
                }
                None => {}
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_notification<T: Notification>(&mut self) -> anyhow::Result<T::Params> {
        loop {
            self.receive()?;
            if let Some(notification) = self.notifications.pop_front() {
                break Ok(serde_json::from_value(notification.params)?);
            }
        }
    }

    /// Attempt to receive a message and either put it in the `responses` map if it's a
    /// response, or the notifications queue if it's a notification.
    ///
    /// Returns an error if an invalid message is received, or if no message is
    /// received within the timeout.
    fn receive(&mut self) -> anyhow::Result<()> {
        let message = self
            .client_connection
            .receiver
            .recv_timeout(self.recv_timeout)?;
        match message {
            Message::Request(req) => {
                Err(anyhow::anyhow!("Got a request from the server: {:?}", req))
            }
            Message::Response(response) => match self.responses.entry(response.id.clone()) {
                Entry::Occupied(existing) => Err(anyhow::anyhow!(
                    "Got a duplicate response for request ID {:?}: Existing: {:?}, New: {:?}",
                    response.id,
                    existing.get(),
                    response
                )),
                Entry::Vacant(entry) => {
                    entry.insert(response);
                    Ok(())
                }
            },
            Message::Notification(notification) => {
                self.notifications.push_back(notification);
                Ok(())
            }
        }
    }

    /// Send a notification saying that a file was opened with the given contents.
    pub fn open_file(&mut self, uri: Url, contents: String) -> anyhow::Result<()> {
        let open_params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: String::new(),
                version: self.next_document_version(),
                text: contents,
            },
        };
        let open_notification = new_notification::<DidOpenTextDocument>(open_params);
        self.send_notification(open_notification)?;
        Ok(())
    }

    /// Send a notification saying that a file was changed with the given contents.
    pub fn change_file(&mut self, uri: Url, contents: String) -> anyhow::Result<()> {
        let change_params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri,
                version: self.next_document_version(),
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: contents,
            }],
        };
        let change_notification = new_notification::<DidChangeTextDocument>(change_params);
        self.send_notification(change_notification)?;
        Ok(())
    }
}