// Copyright 2017 click2stream, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io;

use std::rc::Rc;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::net::ToSocketAddrs;

use bytes::{Bytes, BytesMut};

use futures::task;

use futures::{Async, AsyncSink, Future, Poll, StartSend};
use futures::task::Task;
use futures::stream::Stream;
use futures::sink::Sink;

use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle as TokioCoreHandle;

use tokio_io::AsyncRead;

use futures_ex::StreamEx;

use net::arrow::proto::codec::RawCodec;
use net::arrow::proto::error::ArrowError;
use net::arrow::proto::msg::ArrowMessage;
use net::arrow::proto::msg::control::ControlMessage;

const INPUT_BUFFER_LIMIT: usize  = 32768;
const OUTPUT_BUFFER_LIMIT: usize = 4 * 1024 * 1024 * 1024;

/// Session context.
struct SessionContext {
    service_id:   u16,
    session_id:   u32,
    input:        BytesMut,
    output:       BytesMut,
    input_ready:  Option<Task>,
    input_empty:  Option<Task>,
    output_ready: Option<Task>,
    closed:       bool,
    error:        Option<io::Error>,
}

impl SessionContext {
    /// Create a new session context for a given service ID and session ID.
    fn new(service_id: u16, session_id: u32) -> SessionContext {
        SessionContext {
            service_id:   service_id,
            session_id:   session_id,
            input:        BytesMut::with_capacity(8192),
            output:       BytesMut::with_capacity(8192),
            input_ready:  None,
            input_empty:  None,
            output_ready: None,
            closed:       false,
            error:        None,
        }
    }

    /// Extend the output buffer with data from a given Arrow Message.
    fn push_output_message(&mut self, msg: ArrowMessage) {
        // ignore all incoming messages after the connection gets closed
        if self.closed {
            return
        }

        let data = msg.payload();

        if (self.output.len() + data.len()) > OUTPUT_BUFFER_LIMIT {
            // we cannot backpressure here, so we'll set an error state
            self.set_error(io::Error::new(io::ErrorKind::Other, "output buffer limit exceeded"));
        } else {
            self.output.extend(data);

            // we MUST notify any possible task consuming the output buffer that
            // there is some data available again
            if self.output.len() > 0 {
                if let Some(task) = self.output_ready.take() {
                    task.unpark();
                }
            }
        }
    }

    /// Take all the data from the input buffer and return them as an Arrow
    /// Message. The method returns:
    /// * `Async::Ready(Some(_))` if there was some data available
    /// * `Async::Ready(None)` if there was no data available and the context
    ///   has been closed
    /// * `Async::NotReady` if there was no data available
    fn take_input_message(&mut self) -> Poll<Option<ArrowMessage>, io::Error> {
        let data = self.input.take()
            .freeze();

        // we MUST notify any possible task feeding the input buffer that the
        // buffer is empty again
        if let Some(task) = self.input_empty.take() {
            task.unpark();
        }

        if data.len() > 0 {
            let message = ArrowMessage::new(
                self.service_id,
                self.session_id,
                data);

            Ok(Async::Ready(Some(message)))
        } else if self.closed {
            match self.error.take() {
                Some(err) => Err(err),
                None      => Ok(Async::Ready(None)),
            }
        } else {
            // park the current task and wait until there is some data
            // available in the input buffer
            self.input_ready = Some(task::park());

            Ok(Async::NotReady)
        }
    }

    /// Extend the input buffer with given data. The method returns:
    /// * `AsyncSink::NotReady(_)` with remaining data if the input buffer is
    ///   full
    /// * `AsyncSink::Ready` if all the given data has been inserted into the
    ///   input buffer
    /// * an error if the context has been closed
    fn push_input_data(&mut self, mut msg: Bytes) -> StartSend<Bytes, io::Error> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection has been closed"))
        }

        let mut take = msg.len();

        if (take + self.input.len()) > INPUT_BUFFER_LIMIT {
            take = INPUT_BUFFER_LIMIT - self.input.len();
        }

        self.input.extend(msg.split_to(take));

        // we MUST notify any possible task consuming the input buffer that
        // there is some data available again
        if self.input.len() > 0 {
            if let Some(task) = self.input_ready.take() {
                task.unpark();
            }
        }

        if msg.len() > 0 {
            // park the current task and wait until there is some space in
            // the input buffer again
            self.input_empty = Some(task::park());

            Ok(AsyncSink::NotReady(msg))
        } else {
            Ok(AsyncSink::Ready)
        }
    }

    /// Flush the input buffer. The method returns:
    /// * `Async::Ready(())` if the input buffer is empty
    /// * `Async::NotReady` if the buffer is not empty
    fn flush_input_buffer(&mut self) -> Poll<(), io::Error> {
        if self.input.len() > 0 {
            // park the current task and wait until the input buffer is empty
            self.input_empty = Some(task::park());

            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }

    /// Take data from the output buffer. The method returns:
    /// * `Async::Ready(Some(_))` if there is some data available
    /// * `Async::Ready(None)` if the context has been closed and there is
    ///   not data in the output buffer
    /// * `Async::NotReady` if there is no data available
    fn take_output_data(&mut self) -> Poll<Option<Bytes>, io::Error> {
        let data = self.output.take()
            .freeze();

        if data.len() > 0 {
            Ok(Async::Ready(Some(data)))
        } else if self.closed {
            Ok(Async::Ready(None))
        } else {
            // park the current task and wait until there is some data in
            // the output buffer available again
            self.output_ready = Some(task::park());

            Ok(Async::NotReady)
        }
    }

    /// Mark the context as closed. Note that this method does not flush any
    /// buffer.
    fn close(&mut self) {
        self.closed = true;
    }

    /// Mark the context as closed and set a given error. Note that this
    /// method does not flush any buffer.
    fn set_error(&mut self, err: io::Error) {
        // ignore all errors after the connection gets closed
        if !self.closed {
            self.closed = true;
            self.error  = Some(err);
        }
    }
}

/// Arrow session (i.e. connection to an external service).
struct Session {
    service_id: u16,
    context:    Rc<RefCell<SessionContext>>,
}

impl Session {
    /// Create a new session for a given service ID and session ID.
    fn new(service_id: u16, session_id: u32) -> Session {
        let context = SessionContext::new(service_id, session_id);

        Session {
            service_id: service_id,
            context:    Rc::new(RefCell::new(context))
        }
    }

    /// Push a given Arrow Message into the output buffer.
    fn push(&mut self, msg: ArrowMessage) {
        self.context.borrow_mut()
            .push_output_message(msg)
    }

    /// Take an Arrow Message from the input buffer. The method returns:
    /// * `Async::Ready(Some(_))` if there was some data available
    /// * `Async::Ready(None)` if there was no data available and the context
    ///   has been closed
    /// * `Async::NotReady` if there was no data available
    fn take(&mut self) -> Poll<Option<ArrowMessage>, io::Error> {
        self.context.borrow_mut()
            .take_input_message()
    }

    /// Mark the session as closed. The session context won't accept any new
    /// data, however the buffered data can be still processed. It's up to
    /// the corresponding tasks to consume all remaining data.
    fn close(&mut self) {
        self.context.borrow_mut()
            .close()
    }

    /// Get session transport.
    fn transport(&self) -> SessionTransport {
        SessionTransport {
            context: self.context.clone()
        }
    }

    /// Get session error handler.
    fn error_handler(&self) -> SessionErrorHandler {
        SessionErrorHandler {
            context: self.context.clone()
        }
    }
}

/// Session transport.
struct SessionTransport {
    context: Rc<RefCell<SessionContext>>,
}

impl Stream for SessionTransport {
    type Item  = Bytes;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Bytes>, io::Error> {
        self.context.borrow_mut()
            .take_output_data()
    }
}

impl Sink for SessionTransport {
    type SinkItem  = Bytes;
    type SinkError = io::Error;

    fn start_send(&mut self, data: Bytes) -> StartSend<Bytes, io::Error> {
        self.context.borrow_mut()
            .push_input_data(data)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        self.context.borrow_mut()
            .flush_input_buffer()
    }

    fn close(&mut self) -> Poll<(), io::Error> {
        let mut context = self.context.borrow_mut();

        // mark the context as closed
        context.close();

        // and wait until the input buffer is fully consumed
        context.flush_input_buffer()
    }
}

/// Session error handler.
struct SessionErrorHandler {
    context: Rc<RefCell<SessionContext>>,
}

impl SessionErrorHandler {
    /// Save a given transport error into the session context.
    fn set_error(&mut self, err: io::Error) {
        self.context.borrow_mut()
            .set_error(err)
    }
}

/// Arrow session manager.
pub struct SessionManager {
    tc_handle:  TokioCoreHandle,
    sessions:   HashMap<u32, Session>,
    poll_order: VecDeque<u32>,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new(tc_handle: TokioCoreHandle) -> SessionManager {
        SessionManager {
            tc_handle:  tc_handle,
            sessions:   HashMap::new(),
            poll_order: VecDeque::new(),
        }
    }

    /// Send a given Arrow Message to the corresponding service using a given
    /// session (as specified by the message). The method returns an error
    /// if the session could not be created for some reason.
    pub fn send(&mut self, msg: ArrowMessage) -> Result<(), ArrowError> {
        let header = *msg.header();

        self.get_session_mut(header.service, header.session)?
            .push(msg);

        Ok(())
    }

    /// Get mutable reference to a given session.
    fn get_session_mut(
        &mut self,
        service_id: u16,
        session_id: u32) -> Result<&mut Session, ArrowError> {
        if !self.sessions.contains_key(&session_id) {
            let session = self.connect(service_id, session_id)?;

            self.sessions.insert(
                session_id,
                session);

            self.poll_order.push_back(session_id);
        }

        let session = self.sessions.get_mut(&session_id);

        Ok(session.unwrap())
    }

    /// Connect to a given service and create an associated session object
    /// with a given ID.
    fn connect(
        &mut self,
        service_id: u16,
        session_id: u32) -> Result<Session, ArrowError> {
        // TODO: log session connect
        // TODO: get address of a given service
        let addr = "127.0.0.1:80";
        let addr = addr.to_socket_addrs()?
            .next()
            .ok_or(io::Error::new(io::ErrorKind::Other, "unable to resolve a given address"))?;

        let session = Session::new(service_id, session_id);
        let transport = session.transport();
        let mut err_handler = session.error_handler();

        let client = TcpStream::connect(&addr, &self.tc_handle)
            .and_then(|stream| {
                let framed = stream.framed(RawCodec);
                let (sink, stream) = framed.split();

                let messages = stream.pipe(transport);

                sink.send_all(messages)
            })
            .then(move |res| {
                if let Err(err) = res {
                    err_handler.set_error(err);
                }

                Ok(())
            });

        self.tc_handle.spawn(client);

        Ok(session)
    }

    /// Create a new HUP message.
    fn create_hup_message(
        &mut self,
        service_id: u16,
        session_id: u32,
        error_code: u32) -> ArrowMessage {
        // TODO: we need a reliable way how to get the next control message ID
        let control_msg_id = 0;

        ArrowMessage::new(
            service_id,
            session_id,
            ControlMessage::hup(
                control_msg_id,
                session_id,
                error_code))
    }
}

impl Stream for SessionManager {
    type Item  = ArrowMessage;
    type Error = ArrowError;

    fn poll(&mut self) -> Poll<Option<ArrowMessage>, ArrowError> {
        let mut count = self.poll_order.len();

        while count > 0 {
            if let Some(session_id) = self.poll_order.pop_front() {
                if let Some(mut session) = self.sessions.remove(&session_id) {
                    let service_id = session.service_id;

                    match session.take() {
                        Ok(Async::NotReady) => {
                            self.sessions.insert(session_id, session);
                            self.poll_order.push_back(session_id);
                        },
                        Ok(Async::Ready(None)) => {
                            // TODO: log session close

                            let msg = self.create_hup_message(
                                service_id,
                                session_id,
                                0);

                            return Ok(Async::Ready(Some(msg)))
                        },
                        Ok(Async::Ready(Some(msg))) => {
                            self.sessions.insert(session_id, session);
                            self.poll_order.push_back(session_id);

                            return Ok(Async::Ready(Some(msg)))
                        },
                        Err(err) => {
                            // TODO: log session error

                            let msg = self.create_hup_message(
                                service_id,
                                session_id,
                                0x03);

                            return Ok(Async::Ready(Some(msg)))
                        },
                    }
                }
            }

            count -= 1;
        }

        Ok(Async::NotReady)
    }
}
